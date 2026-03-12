// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Core execution logic shared by both `LocalExecutor` and `OfflineExecutor`.

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use iota_config::{
    transaction_deny_config::TransactionDenyConfig, verifier_signing_config::VerifierSigningConfig,
};
use iota_execution::Executor;
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    base_types::ObjectID,
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    gas::IotaGasStatus,
    gas_coin::NANOS_PER_IOTA,
    metrics::{BytecodeVerifierMetrics, LimitsMetrics},
    object::{MoveObject, Object, Owner},
    storage::BackingStore,
    transaction::{
        InputObjectKind, InputObjects, ObjectReadResult, ReceivingObjectReadResult,
        ReceivingObjects, TransactionData, TransactionDataAPI,
    },
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

/// Configuration for a local execution environment.
pub(crate) struct ExecutionEnv {
    pub(crate) protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    executor: Arc<dyn Executor + Send + Sync>,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
}

impl ExecutionEnv {
    pub(crate) fn new(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
    ) -> Result<Self> {
        let protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);
        let registry = prometheus::Registry::new();
        let limits_metrics = Arc::new(LimitsMetrics::new(&registry));
        let bytecode_verifier_metrics = Arc::new(BytecodeVerifierMetrics::new(&registry));
        let executor = iota_execution::executor(&protocol_config, true, None)?;

        Ok(Self {
            protocol_config,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            executor,
            limits_metrics,
            bytecode_verifier_metrics,
        })
    }
}

/// Run a transaction against the given store: validate, resolve objects, and
/// execute.
///
/// This is the single entry point for both `LocalExecutor` and
/// `OfflineExecutor`.
pub(crate) fn simulate(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    mut transaction: TransactionData,
    checks: VmChecks,
) -> Result<SimulateTransactionResult> {
    transaction.validity_check_no_gas_check(&env.protocol_config)?;

    // Update gas payment references to match actual object versions in the store.
    let updated_gas: Vec<_> = transaction
        .gas()
        .iter()
        .map(|gas_ref| {
            store
                .as_object_store()
                .get_object(&gas_ref.0)
                .map(|obj| obj.compute_object_reference())
                .unwrap_or(*gas_ref)
        })
        .collect();
    transaction.gas_data_mut().payment = updated_gas;

    // Resolve input and receiving objects from the store.
    let raw_input_object_kinds = transaction.input_objects()?;
    let receiving_object_refs = transaction.receiving_objects();

    let (input_object_kinds, mut input_objects) =
        build_input_objects(store, &raw_input_object_kinds)?;
    let receiving_objects = build_receiving_objects(store, &receiving_object_refs)?;

    // Create a mock gas object if the transaction has no gas payment.
    const SIMULATION_GAS_COIN_VALUE: u64 = 1_000_000_000 * NANOS_PER_IOTA;
    let mock_gas_id = if transaction.gas().is_empty() {
        let mock_gas_object = Object::new_move(
            MoveObject::new_gas_coin(1.into(), ObjectID::MAX, SIMULATION_GAS_COIN_VALUE),
            Owner::AddressOwner(transaction.gas_data().owner),
            TransactionDigest::genesis_marker(),
        );
        let mock_gas_object_ref = mock_gas_object.compute_object_reference();
        transaction.gas_data_mut().payment = vec![mock_gas_object_ref];
        input_objects.push(ObjectReadResult::new_from_gas_object(&mock_gas_object));
        Some(mock_gas_object.id())
    } else {
        None
    };

    // Deny-list check.
    let deny_config = TransactionDenyConfig::default();
    let verifier_signing_config = VerifierSigningConfig::default();
    let receiving_object_refs = transaction.receiving_objects();

    iota_transaction_checks::deny::check_transaction_for_signing(
        &transaction,
        &[],
        &input_object_kinds,
        &receiving_object_refs,
        &deny_config,
        store,
    )?;

    // Validate inputs or relax checks for dev-inspect.
    let (gas_status, checked_input_objects) = if checks.enabled() {
        iota_transaction_checks::check_transaction_input(
            &env.protocol_config,
            env.reference_gas_price,
            &transaction,
            input_objects,
            &receiving_objects,
            &env.bytecode_verifier_metrics,
            &verifier_signing_config,
            0, // authenticator_gas_budget
        )?
    } else {
        let checked_input_objects = iota_transaction_checks::check_dev_inspect_input(
            &env.protocol_config,
            transaction.kind(),
            input_objects,
            receiving_objects,
        )?;
        let gas_status = IotaGasStatus::new(
            transaction.gas_budget(),
            transaction.gas_price(),
            env.reference_gas_price,
            &env.protocol_config,
        )?;
        (gas_status, checked_input_objects)
    };

    // Execute the transaction.
    let (kind, signer, gas_coins) = transaction.execution_parts();
    let (inner_temp_store, _, effects, execution_result) = env.executor.dev_inspect_transaction(
        store,
        &env.protocol_config,
        env.limits_metrics.clone(),
        false,
        &HashSet::new(),
        &env.epoch_id,
        env.epoch_timestamp_ms,
        checked_input_objects,
        gas_coins,
        gas_status,
        kind,
        signer,
        transaction.digest(),
        checks.disabled(),
    );

    Ok(SimulateTransactionResult {
        input_objects: inner_temp_store.input_objects,
        output_objects: inner_temp_store.written,
        events: effects.events_digest().map(|_| inner_temp_store.events),
        effects,
        execution_result,
        mock_gas_id,
        suggested_gas_price: None,
    })
}

/// Build `InputObjects` from a store, using the latest fetched versions.
fn build_input_objects(
    store: &dyn BackingStore,
    input_object_kinds: &[InputObjectKind],
) -> Result<(Vec<InputObjectKind>, InputObjects)> {
    let mut updated_kinds = Vec::new();
    let mut input_objects = Vec::new();
    for kind in input_object_kinds {
        let obj = store
            .as_object_store()
            .get_object(&kind.object_id())
            .ok_or_else(|| kind.object_not_found_error())?;

        let updated_kind = match kind {
            InputObjectKind::MovePackage(_) => *kind,
            InputObjectKind::ImmOrOwnedMoveObject(_) => {
                InputObjectKind::ImmOrOwnedMoveObject(obj.compute_object_reference())
            }
            InputObjectKind::SharedMoveObject {
                initial_shared_version,
                mutable,
                ..
            } => InputObjectKind::SharedMoveObject {
                id: obj.id(),
                initial_shared_version: *initial_shared_version,
                mutable: *mutable,
            },
        };

        input_objects.push(ObjectReadResult::new(updated_kind, obj.into()));
        updated_kinds.push(updated_kind);
    }
    Ok((updated_kinds, input_objects.into()))
}

/// Build `ReceivingObjects` from a store.
fn build_receiving_objects(
    store: &dyn BackingStore,
    receiving_object_refs: &[iota_types::base_types::ObjectRef],
) -> Result<ReceivingObjects> {
    let mut receiving_objects = Vec::new();
    for objref in receiving_object_refs {
        let obj = store.as_object_store().get_object(&objref.0).ok_or(
            iota_types::error::UserInputError::ObjectNotFound {
                object_id: objref.0,
                version: Some(objref.1),
            },
        )?;
        let updated_ref = obj.compute_object_reference();
        receiving_objects.push(ReceivingObjectReadResult::new(updated_ref, obj.into()));
    }
    Ok(receiving_objects.into())
}
