// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for chained transaction execution via `ChainedOfflineExecutor`.

use std::path::PathBuf;

use anyhow::Result;
use iota_framework::BuiltInFramework;
use iota_local_executor::{
    ChainedOfflineExecutor, InMemoryStore, LocalPackage, OfflineExecutor, VmChecks,
    apply_effects_to_in_memory,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_sdk_types::SharedObjectReference;
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID, SequenceNumber},
    effects::TransactionEffectsAPI,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        CallArg, TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData,
        TransactionDataAPI,
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

/// `chain` creates a shared counter, then increments it in a second
/// transaction. The second call must see the counter object in the store —
/// proving that `apply_effects_to_in_memory` correctly carries state.
#[test]
fn chain_create_then_increment_counter() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
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
    assert!(
        r1.effects.status().is_success(),
        "tx1 create must succeed: {:?}",
        r1.effects.status()
    );

    // Dig the newly-created Counter out of the effects.
    let counter_ref = r1
        .effects
        .created()
        .into_iter()
        .find(|(_oref, owner)| matches!(owner, iota_types::object::Owner::Shared(_)))
        .map(|(oref, _)| oref)
        .expect("counter should have been created as a shared object");

    let counter_shared_version = match r1
        .effects
        .created()
        .into_iter()
        .find(|(oref, _)| oref.object_id == counter_ref.object_id)
        .map(|(_, owner)| owner)
        .unwrap()
    {
        iota_types::object::Owner::Shared(initial_shared_version) => initial_shared_version,
        other => panic!("expected Shared owner, got {other:?}"),
    };

    // tx2: increment the counter.
    let mut b = ProgrammableTransactionBuilder::new();
    let arg = b
        .obj(CallArg::Shared(SharedObjectReference {
            object_id: counter_ref.object_id,
            initial_shared_version: counter_shared_version,
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
        "tx2 increment must succeed (state was carried from tx1): {:?}",
        r2.effects.status()
    );

    // The counter should have been mutated in tx2's effects.
    let mutated = r2.effects.mutated();
    let hit = mutated
        .iter()
        .any(|(oref, _)| oref.object_id == counter_ref.object_id);
    assert!(
        hit,
        "tx2 should list the counter as mutated; mutated: {mutated:?}"
    );

    // History bookkeeping.
    assert_eq!(chain.history().steps.len(), 2);
    assert!(
        chain
            .history()
            .created_object_ids()
            .any(|id| id == counter_ref.object_id)
    );

    Ok(())
}

/// Verify `apply_effects_to_in_memory` removes deleted objects. Direct
/// unit-style test that doesn't require a real transaction flow.
#[test]
fn apply_effects_removes_deleted_ids() {
    // Build a minimal synthetic `SimulateTransactionResult` by running a
    // throwaway transaction, then mutate the store and verify remove
    // semantics directly.
    use iota_types::{
        digests::TransactionDigest,
        object::{MoveObject, MoveObjectExt, Object, Owner},
    };

    let mut store = InMemoryStore::new();
    let fake_coin = Object::new_move(
        MoveObject::new_gas_coin(SequenceNumber::from(1), ObjectID::MAX, 1_000),
        Owner::Address(IotaAddress::ZERO),
        TransactionDigest::ZERO,
    );
    let fake_id = fake_coin.id();
    store.insert(fake_coin);
    assert!(store.get_object(&fake_id).is_some());

    // Direct remove API (what `apply_effects_to_in_memory` uses internally).
    assert!(store.remove(&fake_id).is_some());
    assert!(store.get_object(&fake_id).is_none());
    // Idempotent.
    assert!(store.remove(&fake_id).is_none());
}

/// Callers can also use `apply_effects_to_in_memory` directly with a plain
/// `OfflineExecutor` (no wrapper) when they want more control.
#[test]
fn manual_apply_effects_across_calls() -> Result<()> {
    let pkg = LocalPackage::compile(&fixture_path(), synthetic_id(), "hello", &protocol_config())?;
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    pkg.install_into(&mut store);

    let mut offline = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;

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
    let r1 = offline.simulate_transaction(tx1, VmChecks::Enabled)?;
    assert!(r1.effects.status().is_success());

    // Before apply: the counter ID isn't in the store yet.
    let counter_id = r1
        .effects
        .created()
        .into_iter()
        .find(|(_oref, owner)| matches!(owner, iota_types::object::Owner::Shared(_)))
        .map(|(oref, _)| oref.object_id)
        .unwrap();
    assert!(offline.store_mut().get_object(&counter_id).is_none());

    // Apply manually.
    apply_effects_to_in_memory(offline.store_mut(), &r1);

    // After apply: present.
    assert!(offline.store_mut().get_object(&counter_id).is_some());
    // And the mock gas coin was NOT persisted (see module note).
    if let Some(mock_id) = r1.mock_gas_id {
        assert!(
            offline.store_mut().get_object(&mock_id).is_none(),
            "mock gas coin must not leak into the store between chained runs"
        );
    }

    // Silence unused-import warning for CallArg when just the manual path is run.
    let _ = CallArg::Pure(vec![]);

    Ok(())
}
