// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for the event-decoding helper (`decode_events`) exposed on each
//! executor. Verifies that a Move event emitted from the fixture module is
//! deserialised into a fully-annotated `MoveValue` with the right field
//! name, value, and struct tag.

use std::path::PathBuf;

use anyhow::Result;
use iota_local_executor::{
    ChainedOfflineExecutor, GasEstimate, InMemoryStore, LocalPackage, OfflineExecutor, VmChecks,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_sdk_types::SharedObjectReference;
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID},
    effects::TransactionEffectsAPI,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        CallArg, TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData,
        TransactionDataAPI,
    },
};
use move_core_types::annotated_value::MoveValue;

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

#[test]
fn decode_emitted_incremented_event() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::with_framework();
    pkg.install_into(&mut store);

    let offline = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;
    let mut chain = ChainedOfflineExecutor::new(offline);

    // tx1: create the counter.
    let mut b = ProgrammableTransactionBuilder::new();
    b.programmable_move_call(
        synthetic_id(),
        Identifier::from_static("counter"),
        Identifier::from_static("create"),
        vec![],
        vec![],
    );
    let tx1 = TransactionData::new_programmable(
        IotaAddress::ZERO,
        vec![],
        b.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );
    let r1 = chain.simulate(tx1, VmChecks::Enabled)?;

    let counter_oref = r1
        .effects
        .created()
        .into_iter()
        .find(|(_oref, owner)| matches!(owner, iota_types::object::Owner::Shared(_)))
        .map(|(oref, _)| oref)
        .unwrap();
    let initial_shared_version = match r1
        .effects
        .created()
        .into_iter()
        .find(|(oref, _)| oref.object_id == counter_oref.object_id)
        .map(|(_, owner)| owner)
        .unwrap()
    {
        iota_types::object::Owner::Shared(initial_shared_version) => initial_shared_version,
        other => panic!("expected Shared, got {other:?}"),
    };

    // tx2: increment — emits the Incremented event.
    let mut b = ProgrammableTransactionBuilder::new();
    let arg = b
        .obj(CallArg::Shared(SharedObjectReference {
            object_id: counter_oref.object_id,
            initial_shared_version,
            mutable: true,
        }))
        .unwrap();
    b.programmable_move_call(
        synthetic_id(),
        Identifier::from_static("counter"),
        Identifier::from_static("increment"),
        vec![],
        vec![arg],
    );
    let tx2 = TransactionData::new_programmable(
        IotaAddress::ZERO,
        vec![],
        b.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );
    let r2 = chain.simulate(tx2, VmChecks::Enabled)?;
    assert!(
        r2.effects.status().is_success(),
        "{:?}",
        r2.effects.status()
    );

    let events = r2.events.as_ref().expect("increment should produce events");
    assert_eq!(events.data.len(), 1, "one event emitted");

    // Decode via the offline executor's helper.
    let decoded = chain.into_inner().decode_events(events);
    assert_eq!(decoded.len(), 1);
    let event = decoded.into_iter().next().unwrap()?;

    assert_eq!(event.package_id, synthetic_id());
    assert_eq!(event.transaction_module.as_str(), "counter");
    assert_eq!(event.type_.name().as_str(), "Incremented");

    // The value must be a struct with one `new_value: u64` field equal to 1
    // (counter was 0, incremented once).
    match &event.value {
        MoveValue::Struct(s) => {
            let new_value = s
                .fields
                .iter()
                .find(|(name, _)| name.as_str() == "new_value")
                .map(|(_, v)| v)
                .expect("Incremented struct should have a `new_value` field");
            match new_value {
                MoveValue::U64(n) => assert_eq!(*n, 1),
                other => panic!("expected U64, got {other:?}"),
            }
        }
        other => panic!("expected MoveValue::Struct, got {other:?}"),
    }

    Ok(())
}

/// Gas estimation pulls the gas-cost summary off the effects and computes
/// `net_gas_usage`. Sanity-check the values are plausible.
#[test]
fn gas_estimate_surfaces_effects_numbers() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::with_framework();
    pkg.install_into(&mut store);

    let offline = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;
    let mut b = ProgrammableTransactionBuilder::new();
    let n = b.pure(20u64).unwrap();
    b.programmable_move_call(
        synthetic_id(),
        Identifier::from_static("hello"),
        Identifier::from_static("work"),
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
    let result = offline.simulate_transaction(tx, VmChecks::Enabled)?;
    let estimate = GasEstimate::from_effects(&result.effects);

    // Computation cost must be > 0 for a non-empty transaction.
    assert!(estimate.computation_cost > 0, "{estimate:?}");
    // Suggested budget with 20% headroom should exceed net_gas_usage.
    let budget = estimate.suggested_budget_with_headroom(1.2);
    if estimate.net_gas_usage > 0 {
        assert!(
            budget >= estimate.net_gas_usage as u64,
            "budget {budget} should be >= net_gas_usage {}",
            estimate.net_gas_usage
        );
    }
    Ok(())
}
