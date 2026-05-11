// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: compile a local Move package, give it a synthetic ID, and run a
//! transaction that calls into it — with debug prints + gas profile + trace.
//!
//! Usage:
//!   cargo run --example local_package_debug
//!
//! Steps:
//!
//! 1. `LocalPackage::compile` reuses the `iota-move-build` pipeline to compile
//!    the fixture Move package at `tests/fixtures/hello_debug/` and binds its
//!    declared `hello` named address to `0x42`.
//! 2. The compiled package is installed into an `InMemoryStore` seeded with the
//!    built-in framework — no network access at any point.
//! 3. A PTB calling `0x42::hello::sum_to(10)` is built and executed via
//!    `OfflineExecutor::with_debug`.
//! 4. The run captures a gas profile (Speedscope JSON) and an instruction
//!    trace, both attached to `DebugArtifacts`.

use std::path::PathBuf;

use anyhow::Result;
use iota_framework::BuiltInFramework;
use iota_local_executor::{
    DebugConfig, InMemoryStore, LocalPackage, OfflineExecutor, ProfileOutput, ProfileSink, VmChecks,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID},
    effects::TransactionEffectsAPI,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData, TransactionDataAPI,
    },
};

fn main() -> Result<()> {
    // Synthetic package ID — any ObjectID works; callers pick something
    // recognisable like 0x42 so it's obvious in traces.
    let synthetic_id = ObjectID::from_short_hex("0x42")?;

    // The fixture package is checked in at tests/fixtures/hello_debug/. The
    // same path works for this example because it's inside the crate.
    let package_path = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures/hello_debug");
        p
    };

    let protocol_config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);

    println!("Compiling local Move package at {package_path:?} under ID {synthetic_id}...");
    let pkg = LocalPackage::compile(&package_path, synthetic_id, "hello", &protocol_config)?;
    println!(
        "  Compiled {} module(s).",
        pkg.compiled.get_modules().count()
    );

    // Seed the store with the framework, then install our synthetic package.
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    // Build `0x42::hello::sum_to(10)` — a simple, non-generic function chosen
    // so the Move VM tracer handles it cleanly.
    let mut builder = ProgrammableTransactionBuilder::new();
    let n_arg = builder.pure(10u64)?;
    builder.programmable_move_call(
        synthetic_id,
        Identifier::from_static("hello"),
        Identifier::from_static("sum_to"),
        vec![],
        vec![n_arg],
    );
    let tx = TransactionData::new_programmable(
        IotaAddress::ZERO,
        vec![], // empty → executor mocks a gas coin
        builder.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    // Run with all three debug toggles on. Debug prints go to stdout in
    // Phase 1 (hence the `[debug]` line you'll see if the fixture is changed
    // to invoke `hello::greet()` instead of `hello::sum_to(10)`).
    let executor = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        1000,
        0,
        0,
        store,
        DebugConfig {
            capture_debug_prints: true,
            profile: Some(ProfileSink::Capture),
            trace: true,
            ..DebugConfig::default()
        },
    )?;

    println!("\nRunning transaction against 0x42::hello::sum_to(10)...");
    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;

    println!("  Effects status: {:?}", out.result.effects.status());
    if let Err(e) = out.result.execution_result {
        eprintln!("  Execution error: {e}");
        return Ok(());
    }

    if let Some(trace) = &out.artifacts.trace {
        println!("  Trace: {} events captured", trace.events.len());
    }

    if let Some(profile) = out.artifacts.profile {
        match profile {
            ProfileOutput::Json(bytes) => {
                // Persist the Speedscope JSON so the developer can open it in
                // https://www.speedscope.app/ — not required for the example
                // but handy.
                let out_path = std::env::temp_dir().join("iota-local-executor-demo-profile.json");
                std::fs::write(&out_path, &bytes)?;
                println!(
                    "  Profile: {} bytes Speedscope JSON written to {}",
                    bytes.len(),
                    out_path.display()
                );
            }
            ProfileOutput::Path(p) => println!("  Profile: written to {}", p.display()),
        }
    }

    Ok(())
}
