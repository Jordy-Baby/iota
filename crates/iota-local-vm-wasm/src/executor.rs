// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Offline executor — ported subset of `iota_local_executor::OfflineExecutor`.
//!
//! Two entry points:
//! - [`OfflineExecutor::simulate_transaction`] runs an unsigned
//!   `TransactionData` (no crypto, no `MoveAuthenticator` resolution).
//! - [`OfflineExecutor::simulate_signed_transaction`] runs a
//!   `SenderSignedData`: verifies the signatures first (standard schemes
//!   cryptographically; a `MoveAuthenticator` by running its function inside
//!   the VM), then executes.

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use iota_config::{
    transaction_deny_config::TransactionDenyConfig, verifier_signing_config::VerifierSigningConfig,
};
use iota_execution::Executor;
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    account_abstraction::{
        account::AuthenticatorFunctionRefV1Key,
        authenticator_function::{
            AuthenticatorFunctionRefForExecution, AuthenticatorFunctionRefV1,
        },
    },
    base_types::ObjectID,
    digests::TransactionDigest,
    dynamic_field::{self, Field},
    effects::TransactionEffectsAPI,
    gas::IotaGasStatus,
    gas_coin::NANOS_PER_IOTA,
    metrics::{BytecodeVerifierMetrics, LimitsMetrics},
    move_authenticator::MoveAuthenticator,
    object::{MoveObject, MoveObjectExt, Object, Owner},
    signature::VerifyParams,
    signature_verification::verify_sender_signed_data_message_signatures,
    storage::BackingStore,
    transaction::{
        CheckedInputObjects, InputObjectKind, InputObjects, ObjectReadResult,
        ReceivingObjectReadResult, ReceivingObjects, SenderSignedData, TransactionData,
        TransactionDataAPI,
    },
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::store::InMemoryStore;

const MOCK_GAS_COIN_NANOS: u64 = 1_000_000_000 * NANOS_PER_IOTA;

pub struct OfflineExecutor {
    protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    executor: Arc<dyn Executor + Send + Sync>,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
    store: InMemoryStore,
}

impl OfflineExecutor {
    pub fn new(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        store: InMemoryStore,
    ) -> Result<Self> {
        let protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);
        let executor = iota_execution::executor(&protocol_config, /* silent */ true, None)?;

        let limits_metrics = Arc::new(new_limits_metrics());
        let bytecode_verifier_metrics = Arc::new(new_bytecode_verifier_metrics());

        Ok(Self {
            protocol_config,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            executor,
            limits_metrics,
            bytecode_verifier_metrics,
            store,
        })
    }

    pub fn store_mut(&mut self) -> &mut InMemoryStore {
        &mut self.store
    }

    /// Simulate an unsigned transaction.
    pub fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let prepared = prepare_transaction(
            &self.protocol_config,
            self.reference_gas_price,
            &self.store,
            transaction,
            checks,
            &self.bytecode_verifier_metrics,
            0,
        )?;
        execute_prepared(
            &self.executor,
            &self.protocol_config,
            self.limits_metrics.clone(),
            self.epoch_id,
            self.epoch_timestamp_ms,
            &self.store,
            prepared,
            checks,
        )
    }

    /// Simulate a **signed** transaction.
    ///
    /// Verifies the signatures first:
    /// - For standard schemes (Ed25519, Secp256k1, Secp256r1, MultiSig,
    ///   Passkey) the full cryptographic check runs before execution.
    /// - For [`MoveAuthenticator`] signatures the sender address is checked
    ///   upfront, then the authenticator function is executed inside the Move
    ///   VM during simulation.
    ///
    /// A signature-verification failure is returned as an error before
    /// execution. A `MoveAuthenticator` that aborts surfaces as a normal
    /// execution failure in the returned [`SimulateTransactionResult`].
    pub fn simulate_signed_transaction(
        &self,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let verify_params = VerifyParams::default();
        verify_sender_signed_data_message_signatures(&signed_data, &verify_params)
            .map_err(|e| anyhow::anyhow!("signature verification: {e}"))?;

        let move_authenticator = signed_data.sender_move_authenticator().cloned();
        let transaction = signed_data.into_inner().intent_message.value;

        let authenticator_gas_budget = match &move_authenticator {
            Some(_) => self.protocol_config.max_auth_gas(),
            None => 0,
        };

        let prepared = prepare_transaction(
            &self.protocol_config,
            self.reference_gas_price,
            &self.store,
            transaction,
            checks,
            &self.bytecode_verifier_metrics,
            authenticator_gas_budget,
        )?;

        match move_authenticator {
            Some(authenticator) => execute_with_move_authenticator(
                &self.executor,
                &self.protocol_config,
                self.limits_metrics.clone(),
                self.epoch_id,
                self.epoch_timestamp_ms,
                &self.store,
                prepared,
                authenticator,
            ),
            None => execute_prepared(
                &self.executor,
                &self.protocol_config,
                self.limits_metrics.clone(),
                self.epoch_id,
                self.epoch_timestamp_ms,
                &self.store,
                prepared,
                checks,
            ),
        }
    }
}

struct PreparedTransaction {
    transaction: TransactionData,
    gas_status: IotaGasStatus,
    checked_input_objects: CheckedInputObjects,
    mock_gas_id: Option<ObjectID>,
}

fn prepare_transaction(
    protocol_config: &ProtocolConfig,
    reference_gas_price: u64,
    store: &dyn BackingStore,
    mut transaction: TransactionData,
    checks: VmChecks,
    bytecode_verifier_metrics: &Arc<BytecodeVerifierMetrics>,
    authenticator_gas_budget: u64,
) -> Result<PreparedTransaction> {
    transaction
        .validity_check_no_gas_check(protocol_config)
        .map_err(|e| anyhow::anyhow!("transaction validity check: {e}"))?;

    // Update gas payment refs to match what's in the store.
    let updated_gas: Vec<_> = transaction
        .gas()
        .iter()
        .map(|gas_ref| {
            store
                .as_object_store()
                .get_object(&gas_ref.object_id)
                .map(|obj| obj.compute_object_reference())
                .unwrap_or(*gas_ref)
        })
        .collect();
    transaction.gas_data_mut().objects = updated_gas;

    let raw_input_object_kinds = transaction.input_objects()?;
    let receiving_object_refs = transaction.receiving_objects();

    let (input_object_kinds, mut input_objects) =
        build_input_objects(store, &raw_input_object_kinds)?;
    let receiving_objects = build_receiving_objects(store, &receiving_object_refs)?;

    let mock_gas_id = if transaction.gas().is_empty() {
        let mock_gas_object = Object::new_move(
            MoveObject::new_gas_coin(1.into(), ObjectID::MAX, MOCK_GAS_COIN_NANOS),
            Owner::Address(transaction.gas_data().owner),
            TransactionDigest::ZERO,
        );
        let mock_gas_object_ref = mock_gas_object.compute_object_reference();
        transaction.gas_data_mut().objects = vec![mock_gas_object_ref];
        input_objects.push(ObjectReadResult::new_from_gas_object(&mock_gas_object));
        Some(mock_gas_object.id())
    } else {
        None
    };

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

    let (gas_status, checked_input_objects) = if checks.enabled() {
        iota_transaction_checks::check_transaction_input(
            protocol_config,
            reference_gas_price,
            &transaction,
            input_objects,
            &receiving_objects,
            bytecode_verifier_metrics,
            &verifier_signing_config,
            authenticator_gas_budget,
        )?
    } else {
        let checked_input_objects = iota_transaction_checks::check_dev_inspect_input(
            protocol_config,
            transaction.kind(),
            input_objects,
            receiving_objects,
        )?;
        let gas_status = IotaGasStatus::new(
            transaction.gas_budget(),
            transaction.gas_price(),
            reference_gas_price,
            protocol_config,
        )?;
        (gas_status, checked_input_objects)
    };

    Ok(PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    })
}

fn execute_prepared(
    executor: &Arc<dyn Executor + Send + Sync>,
    protocol_config: &ProtocolConfig,
    limits_metrics: Arc<LimitsMetrics>,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    checks: VmChecks,
) -> Result<SimulateTransactionResult> {
    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

    let (kind, signer, gas_coins) = transaction.execution_parts();
    let mut trace_builder_opt = None;
    let (inner_temp_store, _, effects, execution_result) = executor.dev_inspect_transaction(
        store,
        protocol_config,
        limits_metrics,
        false,
        &HashSet::new(),
        &epoch_id,
        epoch_timestamp_ms,
        checked_input_objects,
        gas_coins,
        gas_status,
        kind,
        signer,
        transaction.digest(),
        checks.disabled(),
        &mut trace_builder_opt,
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

/// Execute a prepared transaction with a [`MoveAuthenticator`].
///
/// Resolves the authenticator's input objects, loads the
/// `AuthenticatorFunctionRefV1` from the account object's dynamic field, and
/// runs both the authenticator function and the transaction through
/// `authenticate_then_execute_transaction_to_effects`.
fn execute_with_move_authenticator(
    executor: &Arc<dyn Executor + Send + Sync>,
    protocol_config: &ProtocolConfig,
    limits_metrics: Arc<LimitsMetrics>,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    authenticator: MoveAuthenticator,
) -> Result<SimulateTransactionResult> {
    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

    let auth_input_object_kinds = authenticator.input_objects();
    let (_, auth_input_objects) = build_input_objects(store, &auth_input_object_kinds)?;

    let mut union_inputs = checked_input_objects.into_inner();
    for obj in auth_input_objects.iter() {
        if union_inputs.find_object_id_mut(obj.id()).is_none() {
            union_inputs.push(obj.clone());
        }
    }
    let union_checked = CheckedInputObjects::new_with_checked_transaction_inputs(union_inputs);
    let auth_checked = CheckedInputObjects::new_with_checked_transaction_inputs(auth_input_objects);

    let authenticator_fn_ref = resolve_authenticator_function_ref(store, &authenticator)?;

    let tx_data_bytes =
        bcs::to_bytes(&transaction).expect("TransactionData serialization cannot fail");

    let (kind, signer, gas_data) = transaction.execution_parts();
    let mut trace_builder_opt = None;
    let (inner_temp_store, _, effects, execution_result) = executor
        .authenticate_then_execute_transaction_to_effects(
            store,
            protocol_config,
            limits_metrics,
            false,
            &HashSet::new(),
            &epoch_id,
            epoch_timestamp_ms,
            gas_data,
            gas_status,
            vec![(authenticator, authenticator_fn_ref, auth_checked)],
            union_checked,
            kind,
            signer,
            transaction.digest(),
            tx_data_bytes,
            &mut trace_builder_opt,
        );

    Ok(SimulateTransactionResult {
        input_objects: inner_temp_store.input_objects,
        output_objects: inner_temp_store.written,
        events: effects.events_digest().map(|_| inner_temp_store.events),
        effects,
        execution_result: execution_result.map(|_| Vec::new()),
        mock_gas_id,
        suggested_gas_price: None,
    })
}

/// Load the `AuthenticatorFunctionRefV1` from the account object's dynamic
/// field in the store and wrap it for execution. The JS side must have
/// fetched this dynamic-field object — [`auth_function_field_id`] gives it
/// the address.
fn resolve_authenticator_function_ref(
    store: &dyn BackingStore,
    authenticator: &MoveAuthenticator,
) -> Result<AuthenticatorFunctionRefForExecution> {
    let (account_object_id, _version, _digest) = authenticator
        .object_to_authenticate_components()
        .map_err(|e| anyhow::anyhow!("invalid object_to_authenticate: {e}"))?;

    let field_id = auth_function_field_id(account_object_id)?;

    let field_obj = store
        .as_object_store()
        .get_object(&field_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "authenticator function ref not found for account {account_object_id} \
                 (dynamic field {field_id})"
            )
        })?;

    let field_move_object = field_obj
        .data
        .as_struct_opt()
        .ok_or_else(|| anyhow::anyhow!("authenticator dynamic field is not a Move object"))?;

    let field: Field<AuthenticatorFunctionRefV1Key, AuthenticatorFunctionRefV1> = field_move_object
        .to_rust()
        .ok_or_else(|| anyhow::anyhow!("failed to deserialize AuthenticatorFunctionRefV1"))?;

    Ok(AuthenticatorFunctionRefForExecution::new_v1(
        field.value,
        field_obj.compute_object_reference(),
        field_obj.owner,
        field_obj.storage_rebate,
        field_obj.previous_transaction,
    ))
}

/// Derive the on-chain dynamic-field address that stores the
/// `AuthenticatorFunctionRefV1` for `account_object_id`. The JS side calls
/// this so it knows which extra object to fetch when verifying a
/// `MoveAuthenticator` signature.
pub fn auth_function_field_id(account_object_id: ObjectID) -> Result<ObjectID> {
    dynamic_field::derive_dynamic_field_id(
        account_object_id,
        &AuthenticatorFunctionRefV1Key::tag().into(),
        &AuthenticatorFunctionRefV1Key::default().to_bcs_bytes(),
    )
    .map_err(|e| anyhow::anyhow!("derive auth function field id: {e}"))
}

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

// `LimitsMetrics` / `BytecodeVerifierMetrics` constructors differ by target:
// wasm32 has a no-op `new_stub()` (prometheus doesn't compile there), native
// needs a `prometheus::Registry`. We don't read the counters here either way.
#[cfg(target_arch = "wasm32")]
fn new_limits_metrics() -> LimitsMetrics {
    LimitsMetrics::new_stub()
}
#[cfg(target_arch = "wasm32")]
fn new_bytecode_verifier_metrics() -> BytecodeVerifierMetrics {
    BytecodeVerifierMetrics::new_stub()
}
#[cfg(not(target_arch = "wasm32"))]
fn new_limits_metrics() -> LimitsMetrics {
    LimitsMetrics::new(&prometheus::Registry::new())
}
#[cfg(not(target_arch = "wasm32"))]
fn new_bytecode_verifier_metrics() -> BytecodeVerifierMetrics {
    BytecodeVerifierMetrics::new(&prometheus::Registry::new())
}

fn build_receiving_objects(
    store: &dyn BackingStore,
    receiving_object_refs: &[iota_types::base_types::ObjectRef],
) -> Result<ReceivingObjects> {
    let mut receiving_objects = Vec::new();
    for objref in receiving_object_refs {
        let obj = store
            .as_object_store()
            .get_object(&objref.object_id)
            .ok_or(iota_types::error::UserInputError::ObjectNotFound {
                object_id: objref.object_id,
                version: Some(objref.version),
            })?;
        let updated_ref = obj.compute_object_reference();
        receiving_objects.push(ReceivingObjectReadResult::new(updated_ref, obj.into()));
    }
    Ok(receiving_objects.into())
}
