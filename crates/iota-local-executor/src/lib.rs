// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Local Move VM executor for dry-run/dev-inspect without a node.
//!
//! This crate allows running Move VM transactions locally by fetching required
//! objects from a remote node (via JSON-RPC) and executing the transaction
//! using the same execution engine as a full node.
//!
//! Two executor types are provided:
//! - [`LocalExecutor`]: Fetches objects from a remote node on demand.
//! - [`OfflineExecutor`]: Uses only pre-provided objects, no network access.

mod in_memory_store;
mod remote_store;

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
pub use in_memory_store::InMemoryStore;
use iota_config::{
    transaction_deny_config::TransactionDenyConfig, verifier_signing_config::VerifierSigningConfig,
};
use iota_execution::Executor;
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_sdk::IotaClient;
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
pub use remote_store::RemoteStore;

// --------------------------------------------------------------------------
// Shared execution logic
// --------------------------------------------------------------------------

/// Configuration for a local execution environment.
struct ExecutionEnv {
    protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    executor: Arc<dyn Executor + Send + Sync>,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
}

impl ExecutionEnv {
    fn new_from_protocol_config(
        protocol_config: ProtocolConfig,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
    ) -> Result<Self> {
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

/// Execute a transaction against the given store and return the result.
///
/// This is the core execution logic shared by both `LocalExecutor` and
/// `OfflineExecutor`.
fn execute_transaction(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    mut transaction: TransactionData,
    input_object_kinds: Vec<InputObjectKind>,
    mut input_objects: InputObjects,
    receiving_objects: ReceivingObjects,
    checks: VmChecks,
) -> Result<SimulateTransactionResult> {
    // Create a mock gas object if one was not provided
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

    let deny_config = TransactionDenyConfig::default();
    let verifier_signing_config = VerifierSigningConfig::default();
    let receiving_object_refs = transaction.receiving_objects();

    // Check if some transaction elements are denied
    iota_transaction_checks::deny::check_transaction_for_signing(
        &transaction,
        &[],
        &input_object_kinds,
        &receiving_object_refs,
        &deny_config,
        store,
    )?;

    let authenticator_gas_budget = 0;

    let (gas_status, checked_input_objects) = if checks.enabled() {
        iota_transaction_checks::check_transaction_input(
            &env.protocol_config,
            env.reference_gas_price,
            &transaction,
            input_objects,
            &receiving_objects,
            &env.bytecode_verifier_metrics,
            &verifier_signing_config,
            authenticator_gas_budget,
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

/// Build InputObjects from a store, using the latest fetched versions.
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

        // Build an updated InputObjectKind that matches the actual object
        // version, so that ObjectReadResult is consistent.
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

/// Build ReceivingObjects from a store.
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

// --------------------------------------------------------------------------
// LocalExecutor — fetches objects from a remote node
// --------------------------------------------------------------------------

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node.
pub struct LocalExecutor {
    client: IotaClient,
    env: ExecutionEnv,
}

impl LocalExecutor {
    /// Create a new `LocalExecutor` connected to the given IOTA JSON-RPC
    /// endpoint.
    pub async fn new(client: IotaClient) -> Result<Self> {
        let reference_gas_price = client.read_api().get_reference_gas_price().await?;

        let protocol_config_response = client.read_api().get_protocol_config(None).await?;
        let protocol_version = protocol_config_response.protocol_version;
        let protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);

        let latest_checkpoint = client
            .read_api()
            .get_latest_checkpoint_sequence_number()
            .await?;
        let checkpoint = client
            .read_api()
            .get_checkpoint(latest_checkpoint.into())
            .await?;

        let env = ExecutionEnv::new_from_protocol_config(
            protocol_config,
            reference_gas_price,
            checkpoint.epoch,
            checkpoint.timestamp_ms,
        )?;

        Ok(Self { client, env })
    }

    /// Simulate a transaction locally (dev-inspect mode with relaxed checks).
    pub async fn dev_inspect(
        &self,
        transaction: TransactionData,
    ) -> Result<SimulateTransactionResult> {
        self.simulate(transaction, VmChecks::Disabled).await
    }

    /// Simulate a transaction locally (dry-run mode with full checks).
    pub async fn dry_run(&self, transaction: TransactionData) -> Result<SimulateTransactionResult> {
        self.simulate(transaction, VmChecks::Enabled).await
    }

    async fn simulate(
        &self,
        mut transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        transaction.validity_check_no_gas_check(&self.env.protocol_config)?;

        let store = RemoteStore::new(self.client.clone());

        // Fetch all required objects from the remote node (latest versions)
        self.prefetch_objects(&store, &transaction).await?;

        // Update gas payment references to match fetched versions
        let updated_gas: Vec<_> = transaction
            .gas()
            .iter()
            .map(|gas_ref| {
                if let Some(obj) = store.get_object(&gas_ref.0) {
                    obj.compute_object_reference()
                } else {
                    *gas_ref
                }
            })
            .collect();
        transaction.gas_data_mut().payment = updated_gas;

        let raw_input_object_kinds = transaction.input_objects()?;
        let receiving_object_refs = transaction.receiving_objects();

        let (input_object_kinds, input_objects) =
            build_input_objects(&store, &raw_input_object_kinds)?;
        let receiving_objects = build_receiving_objects(&store, &receiving_object_refs)?;

        execute_transaction(
            &self.env,
            &store,
            transaction,
            input_object_kinds,
            input_objects,
            receiving_objects,
            checks,
        )
    }

    /// Fetch all objects referenced by the transaction from the remote node.
    async fn prefetch_objects(
        &self,
        store: &RemoteStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        use iota_json_rpc_types::IotaObjectDataOptions;

        let input_object_kinds = transaction.input_objects()?;
        let receiving_object_refs = transaction.receiving_objects();

        let options = IotaObjectDataOptions::full_content()
            .with_bcs()
            .with_owner()
            .with_previous_transaction();

        // Collect all object IDs to fetch
        let mut object_ids: Vec<ObjectID> = input_object_kinds
            .iter()
            .map(|kind| kind.object_id())
            .collect();

        // Add gas payment object IDs
        for gas_ref in transaction.gas() {
            if !object_ids.contains(&gas_ref.0) {
                object_ids.push(gas_ref.0);
            }
        }

        // Add receiving object IDs
        for objref in &receiving_object_refs {
            if !object_ids.contains(&objref.0) {
                object_ids.push(objref.0);
            }
        }

        // Deduplicate
        object_ids.sort();
        object_ids.dedup();

        // Fetch all objects in a single batch call
        if !object_ids.is_empty() {
            let responses = self
                .client
                .read_api()
                .multi_get_object_with_options(object_ids, options)
                .await?;

            for response in responses {
                if let Some(data) = response.data {
                    let obj: Object = data.try_into()?;
                    store.insert(obj);
                }
            }
        }

        Ok(())
    }
}

// --------------------------------------------------------------------------
// OfflineExecutor — fully offline, all objects provided upfront
// --------------------------------------------------------------------------

/// A fully offline executor that runs Move VM transactions using only
/// pre-provided objects. No network access is performed.
///
/// # Example
///
/// ```rust,no_run
/// use iota_local_executor::{InMemoryStore, OfflineExecutor};
/// use iota_protocol_config::ProtocolVersion;
///
/// let store = InMemoryStore::new();
/// // store.insert(obj1);
/// // store.insert(obj2);
///
/// let executor = OfflineExecutor::new(
///     ProtocolVersion::MAX,
///     1000, // reference_gas_price
///     0,    // epoch_id
///     0,    // epoch_timestamp_ms
///     store,
/// )
/// .unwrap();
/// ```
pub struct OfflineExecutor {
    env: ExecutionEnv,
    store: InMemoryStore,
}

impl OfflineExecutor {
    /// Create a new fully offline executor.
    ///
    /// - `protocol_version`: The protocol version to use for execution.
    /// - `reference_gas_price`: The reference gas price for the epoch.
    /// - `epoch_id`: The epoch ID to simulate under.
    /// - `epoch_timestamp_ms`: The epoch timestamp in milliseconds.
    /// - `store`: A pre-populated `InMemoryStore` with all objects needed for
    ///   execution (packages, input objects, etc.).
    pub fn new(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        store: InMemoryStore,
    ) -> Result<Self> {
        let protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);
        let env = ExecutionEnv::new_from_protocol_config(
            protocol_config,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
        )?;
        Ok(Self { env, store })
    }

    /// Get a mutable reference to the underlying store, allowing insertion of
    /// additional objects before execution.
    pub fn store_mut(&mut self) -> &mut InMemoryStore {
        &mut self.store
    }

    /// Simulate a transaction offline (dev-inspect mode with relaxed checks).
    pub fn dev_inspect(&self, transaction: TransactionData) -> Result<SimulateTransactionResult> {
        self.simulate(transaction, VmChecks::Disabled)
    }

    /// Simulate a transaction offline (dry-run mode with full checks).
    pub fn dry_run(&self, transaction: TransactionData) -> Result<SimulateTransactionResult> {
        self.simulate(transaction, VmChecks::Enabled)
    }

    fn simulate(
        &self,
        mut transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        transaction.validity_check_no_gas_check(&self.env.protocol_config)?;

        // Update gas payment references to match stored object versions
        let updated_gas: Vec<_> = transaction
            .gas()
            .iter()
            .map(|gas_ref| {
                if let Some(obj) = self.store.get_object(&gas_ref.0) {
                    obj.compute_object_reference()
                } else {
                    *gas_ref
                }
            })
            .collect();
        transaction.gas_data_mut().payment = updated_gas;

        let raw_input_object_kinds = transaction.input_objects()?;
        let receiving_object_refs = transaction.receiving_objects();

        let (input_object_kinds, input_objects) =
            build_input_objects(&self.store, &raw_input_object_kinds)?;
        let receiving_objects = build_receiving_objects(&self.store, &receiving_object_refs)?;

        execute_transaction(
            &self.env,
            &self.store,
            transaction,
            input_object_kinds,
            input_objects,
            receiving_objects,
            checks,
        )
    }
}
