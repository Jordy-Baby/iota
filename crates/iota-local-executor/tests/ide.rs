// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for the IDE-integration post-processor.

use std::path::PathBuf;

use anyhow::Result;
use iota_local_executor::{
    DebugConfig, InMemoryStore, LocalPackage, OfflineExecutor, VmChecks, summarize_by_function,
    summarize_with_source,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData, TransactionDataAPI,
    },
};

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/hello_debug");
    p
}

fn synthetic_id() -> ObjectID {
    ObjectID::from_short_hex("0x42").unwrap()
}

fn protocol_config() -> ProtocolConfig {
    ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown)
}

/// Run `hello::sum_to(50)` with tracing on, then aggregate. The `sum_to`
/// function must show up with a non-zero gas bill and at least one call.
#[test]
fn summarize_by_function_sees_fixture_calls() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::with_framework();
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

    let mut b = ProgrammableTransactionBuilder::new();
    let n = b.pure(50u64).unwrap();
    b.programmable_move_call(
        synthetic_id(),
        Identifier::from_static("hello"),
        Identifier::from_static("sum_to"),
        vec![],
        vec![n],
    );
    let tx = TransactionData::new_programmable(
        IotaAddress::ZERO,
        vec![],
        b.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );
    let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
    let trace = out.artifacts.trace.expect("trace must be present");

    // Aggregate without source.
    let summaries = summarize_by_function(&trace);
    let sum_to = summaries
        .iter()
        .find(|s| s.function_name == "sum_to" && s.module.name().as_str() == "hello")
        .expect("sum_to should appear in the aggregation");
    assert_eq!(sum_to.calls, 1);
    assert!(sum_to.instructions > 0, "sum_to: {sum_to:?}");
    assert!(sum_to.gas_used > 0, "sum_to: {sum_to:?}");
    assert!(sum_to.source.is_none(), "no source map was supplied");

    // Aggregate with source — source span should resolve.
    let with_source = summarize_with_source(&trace, &pkg.compiled);
    let sum_to_src = with_source
        .iter()
        .find(|s| s.function_name == "sum_to" && s.module.name().as_str() == "hello")
        .expect("sum_to present");
    let span = sum_to_src
        .source
        .as_ref()
        .expect("sum_to should have a source span since it's in the supplied CompiledPackage");
    assert!(span.start_byte < span.end_byte, "span: {span:?}");

    Ok(())
}
