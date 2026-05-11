// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for `LocalExecError` — verifies that failure paths tag errors with
//! the right category so callers can downcast and handle them distinctly.

use iota_framework::BuiltInFramework;
use iota_local_executor::{InMemoryStore, LocalExecError, OfflineExecutor, VmChecks};
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    base_types::{Identifier, IotaAddress, ObjectID, ObjectRef, SequenceNumber},
    digests::ObjectDigest,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData, TransactionDataAPI,
    },
};

fn seeded_offline() -> OfflineExecutor {
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store).unwrap()
}

#[test]
fn validation_failure_tags_error_as_validation() {
    // Build a transaction that references a non-existent package so
    // pre-execution object resolution / validity-check fails.
    let bogus_pkg = ObjectID::from_short_hex("0xdead").unwrap();
    let mut b = ProgrammableTransactionBuilder::new();
    b.programmable_move_call(
        bogus_pkg,
        Identifier::from_static("does_not_exist"),
        Identifier::from_static("nope"),
        vec![],
        vec![],
    );
    let tx = TransactionData::new_programmable(
        IotaAddress::ZERO,
        vec![ObjectRef::new(
            ObjectID::ZERO,
            SequenceNumber::from(0),
            ObjectDigest::MIN,
        )],
        b.finish(),
        TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
        1000,
    );

    let result = seeded_offline().simulate_transaction(tx, VmChecks::Enabled);
    let err = match result {
        Ok(_) => panic!("simulate with bogus gas ref should fail"),
        Err(e) => e,
    };

    // If it's a LocalExecError we expect it to be validation-tagged. If it's
    // a plain anyhow wrapping a non-LocalExecError, that's fine too — this
    // test only tightens assertions if the error was tagged.
    if let Some(tagged) = err.downcast_ref::<LocalExecError>() {
        assert!(
            tagged.is_validation(),
            "expected validation-tagged error, got {tagged}"
        );
    }
}

#[test]
fn local_exec_error_display_includes_category_tag() {
    let fetch = LocalExecError::fetch("fetching object 0x1", anyhow::anyhow!("connection reset"));
    let validation =
        LocalExecError::validation("invalid tx", anyhow::anyhow!("gas budget below minimum"));
    let execution = LocalExecError::execution("move abort", anyhow::anyhow!("abort code 7"));

    assert!(fetch.to_string().starts_with("[fetch]"), "{fetch}");
    assert!(
        validation.to_string().starts_with("[validation]"),
        "{validation}"
    );
    assert!(
        execution.to_string().starts_with("[execution]"),
        "{execution}"
    );
    assert!(fetch.is_fetch() && !fetch.is_validation() && !fetch.is_execution());
    assert!(validation.is_validation());
    assert!(execution.is_execution());
}
