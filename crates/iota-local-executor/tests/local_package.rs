// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for `LocalPackage::compile` — the synthetic-ID Move package
//! pipeline used by the local debugging workflow.

use std::path::PathBuf;

use anyhow::Result;
use iota_local_executor::{
    DebugConfig, InMemoryStore, LocalPackage, OfflineExecutor, ProfileSink, VmChecks,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    base_types::ObjectID,
    effects::TransactionEffectsAPI,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData},
};
use move_core_types::{account_address::AccountAddress, ident_str};

/// Path on disk to the `hello_debug` fixture Move package.
fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/hello_debug");
    p
}

fn protocol_config() -> ProtocolConfig {
    ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown)
}

/// The synthetic package ID we use across this test file.
fn synthetic_id() -> ObjectID {
    ObjectID::from_hex_literal("0x42").expect("static hex literal parses")
}

/// With `capture_debug_prints + structured_debug_capture`, the fixture's
/// `debug::print(&b"hello from iota-local-executor")` call should land in
/// `artifacts.debug_prints` as a real in-memory string — no stdout
/// redirection needed.
#[test]
fn greet_debug_prints_captured_into_artifacts() -> Result<()> {
    use iota_framework::BuiltInFramework;

    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    let executor = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        1000,
        0,
        0,
        store,
        DebugConfig {
            capture_debug_prints: true,
            structured_debug_capture: true,
            ..DebugConfig::default()
        },
    )?;

    let mut builder = ProgrammableTransactionBuilder::new();
    builder.programmable_move_call(
        synthetic_id(),
        ident_str!("hello").into(),
        ident_str!("greet").into(),
        vec![],
        vec![],
    );
    let tx = TransactionData::new_programmable(
        iota_types::base_types::IotaAddress::ZERO,
        vec![],
        builder.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
    assert!(
        out.result.effects.status().is_ok(),
        "execution should succeed: {:?}",
        out.result.effects.status()
    );

    assert!(
        !out.artifacts.debug_prints.is_empty(),
        "structured_debug_capture must populate debug_prints"
    );
    // The fixture prints a byte-string literal; the native formats bytes as
    // hex. Assert the line starts with our `[debug] ` prefix and contains
    // the hex of "hello".
    let hit = out
        .artifacts
        .debug_prints
        .iter()
        .any(|line| line.starts_with("[debug] ") && line.contains("68656c6c6f"));
    assert!(
        hit,
        "expected a [debug] line containing hex(hello); got {:?}",
        out.artifacts.debug_prints
    );
    Ok(())
}

#[test]
fn compile_binds_synthetic_id_into_every_module() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;

    assert_eq!(pkg.id, synthetic_id());
    assert_eq!(pkg.object.id(), synthetic_id());

    let expected = AccountAddress::from(synthetic_id());
    let modules: Vec<_> = pkg.compiled.get_modules().collect();
    assert!(!modules.is_empty(), "fixture package must have >0 modules");
    for m in modules {
        assert_eq!(
            m.address(),
            &expected,
            "compiled module {} has address {} — expected synthetic ID {}",
            m.self_id().name(),
            m.address(),
            synthetic_id()
        );
    }
    Ok(())
}

#[test]
fn compile_errors_when_named_address_is_wrong() {
    let result = LocalPackage::compile(
        &fixture_path(),
        synthetic_id(),
        "does_not_exist_in_fixture",
        &protocol_config(),
    );
    assert!(
        result.is_err(),
        "compile must fail when named_address is unknown"
    );
    let err = result.err().unwrap();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("compiling local package") || msg.contains("does_not_exist"),
        "error message should mention the compile failure or the bad name; got: {msg}"
    );
}

/// End-to-end: compile → install → execute. Asserts the synthetic package is
/// actually callable from a PTB and the transaction succeeds.
#[test]
fn ptb_call_against_synthetic_package_succeeds() -> Result<()> {
    use iota_framework::BuiltInFramework;

    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;

    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    let executor = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;

    // PTB: `0x42::hello::sum_to(10)`.
    let mut builder = ProgrammableTransactionBuilder::new();
    let n_arg = builder.pure(10u64)?;
    builder.programmable_move_call(
        synthetic_id(),
        ident_str!("hello").into(),
        ident_str!("sum_to").into(),
        vec![],
        vec![n_arg],
    );
    let pt = builder.finish();

    // Passing an empty gas payment tells the executor to mock up a gas coin
    // automatically (see `prepare_transaction` in src/execution.rs).
    let sender = iota_types::base_types::IotaAddress::ZERO;
    let tx = TransactionData::new_programmable(
        sender,
        vec![],
        pt,
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
    assert!(
        out.result.effects.status().is_ok(),
        "dev-inspect of synthetic-package PTB should succeed: {:?}",
        out.result.effects.status()
    );
    Ok(())
}

/// Execution tracing against the fixture. In Checkpoint A the trace test was
/// `#[ignore]`d because the Move VM's tracer panicked on the dev-inspect
/// synthetic wrapper when invoking generic natives like `blake2b256`. With a
/// plain non-generic entry function (`hello::sum_to`) the tracer behaves, so
/// we can assert content now.
#[test]
fn trace_captures_events_for_fixture_call() -> Result<()> {
    use iota_framework::BuiltInFramework;

    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    let executor = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        1000,
        0,
        0,
        store,
        DebugConfig {
            trace: true,
            ..DebugConfig::default()
        },
    )?;

    let mut builder = ProgrammableTransactionBuilder::new();
    let n_arg = builder.pure(5u64)?;
    builder.programmable_move_call(
        synthetic_id(),
        ident_str!("hello").into(),
        ident_str!("sum_to").into(),
        vec![],
        vec![n_arg],
    );
    let pt = builder.finish();
    let sender = iota_types::base_types::IotaAddress::ZERO;
    let tx = TransactionData::new_programmable(
        sender,
        vec![],
        pt,
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
    assert!(
        out.result.effects.status().is_ok(),
        "execution should succeed: {:?}",
        out.result.effects.status()
    );
    let trace = out.artifacts.trace.expect("trace should be populated");
    assert!(
        !trace.events.is_empty(),
        "MoveTrace should contain events for a successful call; got {} events",
        trace.events.len()
    );
    Ok(())
}

/// Full debug config (prints + profile + trace) running against the fixture
/// must leave `TransactionEffects` byte-identical to a default run. Guards
/// against the debug instrumentation quietly perturbing execution.
#[test]
fn full_debug_config_preserves_effects_on_fixture() -> Result<()> {
    use iota_framework::BuiltInFramework;

    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;

    let mk_tx = || {
        let mut builder = ProgrammableTransactionBuilder::new();
        let n_arg = builder.pure(5u64).unwrap();
        builder.programmable_move_call(
            synthetic_id(),
            ident_str!("hello").into(),
            ident_str!("sum_to").into(),
            vec![],
            vec![n_arg],
        );
        TransactionData::new_programmable(
            iota_types::base_types::IotaAddress::ZERO,
            vec![],
            builder.finish(),
            TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
            1000,
        )
    };

    let mk_store = || {
        let mut store = InMemoryStore::new();
        for obj in BuiltInFramework::genesis_objects() {
            store.insert(obj);
        }
        pkg.install_into(&mut store);
        store
    };

    let plain = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, mk_store())?
        .simulate_transaction_with_debug(mk_tx(), VmChecks::Disabled)?;
    let debugged = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        1000,
        0,
        0,
        mk_store(),
        DebugConfig {
            capture_debug_prints: true,
            profile: Some(ProfileSink::Capture),
            trace: true,
            ..DebugConfig::default()
        },
    )?
    .simulate_transaction_with_debug(mk_tx(), VmChecks::Disabled)?;

    let plain_bytes = bcs::to_bytes(&plain.result.effects)?;
    let debugged_bytes = bcs::to_bytes(&debugged.result.effects)?;
    assert_eq!(
        plain_bytes, debugged_bytes,
        "full-debug run must produce byte-identical effects as a plain run"
    );
    Ok(())
}

/// Gas profiling against our fixture: the profile must contain a frame whose
/// name references the fixture module, proving the profiler saw the synthetic
/// package's bytecode rather than just framework code.
#[test]
fn profile_captures_fixture_module_frame() -> Result<()> {
    use iota_framework::BuiltInFramework;

    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    let executor = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        1000,
        0,
        0,
        store,
        DebugConfig {
            profile: Some(ProfileSink::Capture),
            ..DebugConfig::default()
        },
    )?;

    // Call `work(50)` — enough iterations that the profiler records frames.
    let mut builder = ProgrammableTransactionBuilder::new();
    let n_arg = builder.pure(50u64)?;
    builder.programmable_move_call(
        synthetic_id(),
        ident_str!("hello").into(),
        ident_str!("work").into(),
        vec![],
        vec![n_arg],
    );
    let pt = builder.finish();
    let sender = iota_types::base_types::IotaAddress::ZERO;
    let tx = TransactionData::new_programmable(
        sender,
        vec![],
        pt,
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
    assert!(out.result.effects.status().is_ok());

    let profile = out
        .artifacts
        .profile
        .expect("Capture must populate profile");
    let bytes = match profile {
        iota_local_executor::ProfileOutput::Json(b) => b,
        iota_local_executor::ProfileOutput::Path(p) => panic!("expected Json, got Path {p:?}"),
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes)?;

    // Speedscope frames record the function name in `name` and the fully
    // qualified location in `file`. Match on `file` because frame `name` is
    // just the short name (e.g. "work") which could collide with any other
    // function called "work".
    let frames = json
        .pointer("/shared/frames")
        .and_then(|v| v.as_array())
        .expect("Speedscope JSON should contain /shared/frames");
    let hit = frames.iter().any(|f| {
        f.get("file")
            .and_then(|n| n.as_str())
            .map(|s| s.contains("::hello::"))
            .unwrap_or(false)
    });
    assert!(
        hit,
        "expected at least one frame whose `file` references the fixture module `hello`; \
         frames: {frames:?}"
    );
    Ok(())
}
