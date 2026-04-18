// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Core execution logic shared by both `JsonRpcExecutor` and `OfflineExecutor`.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

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
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    dynamic_field::{self, Field},
    effects::TransactionEffectsAPI,
    gas::IotaGasStatus,
    gas_coin::NANOS_PER_IOTA,
    metrics::{BytecodeVerifierMetrics, LimitsMetrics},
    move_authenticator::MoveAuthenticator,
    object::{MoveObject, Object, Owner},
    signature::VerifyParams,
    signature_verification::{VerifiedDigestCache, verify_sender_signed_data_message_signatures},
    storage::BackingStore,
    transaction::{
        CheckedInputObjects, InputObjectKind, InputObjects, ObjectReadResult,
        ReceivingObjectReadResult, ReceivingObjects, SenderSignedData, TransactionData,
        TransactionDataAPI,
    },
    transaction_executor::{SimulateTransactionResult, VmChecks},
};
use move_trace_format::format::MoveTraceBuilder;

use crate::debug::{DebugArtifacts, DebugConfig, DebugSimulateResult, ProfileOutput, ProfileSink};

/// Configuration for a local execution environment.
pub(crate) struct ExecutionEnv {
    pub(crate) protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    executor: Arc<dyn Executor + Send + Sync>,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
    debug_config: DebugConfig,
    /// For `ProfileSink::Capture`: a dedicated temp directory the VM profiler
    /// writes into; after execution we scan it for the generated JSON file.
    /// `None` when `debug_config.profile` is `None` or `ProfileSink::File`.
    capture_profile_dir: Option<PathBuf>,
}

impl ExecutionEnv {
    pub(crate) fn with_debug(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        debug_config: DebugConfig,
    ) -> Result<Self> {
        let protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);
        let registry = prometheus::Registry::new();
        let limits_metrics = Arc::new(LimitsMetrics::new(&registry));
        let bytecode_verifier_metrics = Arc::new(BytecodeVerifierMetrics::new(&registry));

        // Resolve the profile path. `File` forwards the caller's path directly.
        // `Capture` puts the profiler into a dedicated temp directory — the VM
        // writes `<our-path>_<frame-name>_<timestamp>.<ext>` rather than the exact
        // path, so we scan the directory afterwards instead of guessing the name.
        let (profile_path, capture_profile_dir) = match &debug_config.profile {
            Some(ProfileSink::File(p)) => (Some(p.clone()), None),
            Some(ProfileSink::Capture) => {
                let dir = profile_capture_dir();
                std::fs::create_dir_all(&dir)?;
                (Some(dir.join("profile.json")), Some(dir))
            }
            None => (None, None),
        };

        let silent = !debug_config.capture_debug_prints;
        let executor = iota_execution::executor(&protocol_config, silent, profile_path)?;

        Ok(Self {
            protocol_config,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            executor,
            limits_metrics,
            bytecode_verifier_metrics,
            debug_config,
            capture_profile_dir,
        })
    }

    pub(crate) fn trace_enabled(&self) -> bool {
        self.debug_config.trace
    }

    /// Get a `LayoutResolver` built from this env's Move VM and the supplied
    /// type-layout store. Used by event-decoding helpers on each executor.
    pub(crate) fn type_layout_resolver<'r, 'store: 'r>(
        &'r self,
        store: Box<dyn iota_types::execution::TypeLayoutStore + 'store>,
    ) -> Box<dyn iota_types::layout_resolver::LayoutResolver + 'r> {
        self.executor.type_layout_resolver(store)
    }

    /// Materialise the captured artifacts for the current run: read the
    /// profile JSON back from disk if the sink was [`ProfileSink::Capture`],
    /// drain the thread-local `debug::print` sink if one was installed, and
    /// attach the finished trace if one was requested.
    fn collect_artifacts(&self, trace_builder: Option<MoveTraceBuilder>) -> DebugArtifacts {
        let profile = match (&self.debug_config.profile, &self.capture_profile_dir) {
            (Some(ProfileSink::File(p)), _) => Some(ProfileOutput::Path(p.clone())),
            (Some(ProfileSink::Capture), Some(dir)) => merge_profile_dir(dir),
            _ => None,
        };

        let debug_prints = if self.debug_config.structured_debug_capture {
            move_stdlib_natives::debug::take_debug_sink()
        } else {
            Vec::new()
        };

        DebugArtifacts {
            debug_prints,
            profile,
            trace: trace_builder.map(|b| b.into_trace()),
        }
    }
}

impl Drop for ExecutionEnv {
    fn drop(&mut self) {
        if let Some(dir) = self.capture_profile_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Allocate a unique temp directory for captured gas-profile output. The VM
/// profiler writes `<path>_<frame-name>_<timestamp>.<ext>` into this dir, and
/// we scan it back after execution.
fn profile_capture_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("iota-local-executor-gas-profile-{pid}-{n}"))
}

/// Merge every Speedscope JSON file written into `dir` by the Move VM
/// profiler into a single Speedscope document.
///
/// The profiler writes one file per VM invocation — e.g. a transaction with a
/// `MoveAuthenticator` produces two files: one for the authenticator call and
/// one for the PTB body. Each file has its own `shared.frames` array and a
/// single-element `profiles` array; the merged document preserves every
/// profile while rebuilding a de-duplicated frames table.
///
/// Returns `None` if the directory is missing or contains no JSON files.
fn merge_profile_dir(dir: &std::path::Path) -> Option<ProfileOutput> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut docs: Vec<serde_json::Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(doc) => docs.push(doc),
                Err(e) => {
                    tracing::warn!("Failed to parse captured gas profile at {path:?}: {e}")
                }
            },
            Err(e) => tracing::warn!("Failed to read captured gas profile at {path:?}: {e}"),
        }
    }
    if docs.is_empty() {
        return None;
    }
    // Fast path: exactly one file — return it unchanged.
    if docs.len() == 1 {
        return serde_json::to_vec(&docs[0]).ok().map(ProfileOutput::Json);
    }
    // Merge: concatenate `profiles` arrays and rebuild `shared.frames` as the
    // de-duplicated union. Each profile's event frame-indices are rewritten
    // to point into the merged table.
    let mut merged_frames: Vec<serde_json::Value> = Vec::new();
    let mut merged_profiles: Vec<serde_json::Value> = Vec::new();
    for mut doc in docs {
        let Some(frames) = doc
            .pointer("/shared/frames")
            .and_then(|v| v.as_array())
            .cloned()
        else {
            continue;
        };
        let mut index_map: Vec<usize> = Vec::with_capacity(frames.len());
        for frame in frames {
            let existing = merged_frames.iter().position(|f| f == &frame);
            index_map.push(existing.unwrap_or_else(|| {
                merged_frames.push(frame);
                merged_frames.len() - 1
            }));
        }
        if let Some(profiles) = doc.get_mut("profiles").and_then(|v| v.as_array_mut()) {
            for profile in profiles.iter_mut() {
                if let Some(events) = profile.get_mut("events").and_then(|v| v.as_array_mut()) {
                    for ev in events.iter_mut() {
                        if let Some(frame) =
                            ev.get("frame").and_then(|v| v.as_u64()).map(|i| i as usize)
                        {
                            if let Some(new) = index_map.get(frame) {
                                ev["frame"] = serde_json::json!(new);
                            }
                        }
                    }
                }
                merged_profiles.push(profile.clone());
            }
        }
    }
    let merged = serde_json::json!({
        "exporter": "iota-local-executor (merged)",
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "shared": { "frames": merged_frames },
        "profiles": merged_profiles,
    });
    serde_json::to_vec(&merged).ok().map(ProfileOutput::Json)
}

/// Run a transaction against the given store: validate, resolve objects, and
/// execute.
///
/// This is the single entry point for both `JsonRpcExecutor` and
/// `OfflineExecutor`.
pub(crate) fn simulate(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    transaction: TransactionData,
    checks: VmChecks,
) -> Result<SimulateTransactionResult> {
    simulate_with_debug(env, store, transaction, checks).map(|d| d.result)
}

/// Run a transaction and return the captured debug artifacts alongside the
/// result. Artifact population is controlled by the [`DebugConfig`] stored on
/// the [`ExecutionEnv`] — with a default config this behaves like [`simulate`]
/// plus an empty [`DebugArtifacts`].
pub(crate) fn simulate_with_debug(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    transaction: TransactionData,
    checks: VmChecks,
) -> Result<DebugSimulateResult> {
    if env.debug_config.structured_debug_capture {
        move_stdlib_natives::debug::install_debug_sink();
    }
    let prepared = prepare_transaction(env, store, transaction, checks, 0)?;
    let mut trace_builder = env.trace_enabled().then(MoveTraceBuilder::new);
    let result = execute_prepared(env, store, prepared, checks, &mut trace_builder)?;
    let artifacts = env.collect_artifacts(trace_builder);
    Ok(DebugSimulateResult { result, artifacts })
}

/// Run a **signed** transaction: verify signatures first, then simulate.
///
/// For standard signature schemes (Ed25519, Secp256k1, Secp256r1, MultiSig)
/// the cryptographic verification happens entirely here, before execution.
///
/// For [`MoveAuthenticator`] signatures, the sender address is verified
/// first, then the full authenticator function is executed inside the Move
/// VM during transaction simulation. This makes `simulate_signed` the only
/// way to fully verify a `MoveAuthenticator` signature.
///
/// If signature verification fails the error is returned immediately,
/// without executing the transaction.
pub(crate) fn simulate_signed(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    signed_data: SenderSignedData,
    checks: VmChecks,
) -> Result<SimulateTransactionResult> {
    simulate_signed_with_debug(env, store, signed_data, checks).map(|d| d.result)
}

/// Signed-transaction variant of [`simulate_with_debug`].
pub(crate) fn simulate_signed_with_debug(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    signed_data: SenderSignedData,
    checks: VmChecks,
) -> Result<DebugSimulateResult> {
    if env.debug_config.structured_debug_capture {
        move_stdlib_natives::debug::install_debug_sink();
    }
    let verify_params = VerifyParams::default();
    let zklogin_inputs_cache = Arc::new(VerifiedDigestCache::new_empty());

    verify_sender_signed_data_message_signatures(
        &signed_data,
        env.epoch_id,
        &verify_params,
        zklogin_inputs_cache,
    )
    .map_err(|e| {
        crate::error::LocalExecError::validation("signature verification", anyhow::anyhow!("{e}"))
    })?;

    let move_authenticator = signed_data.sender_move_authenticator().cloned();
    let transaction = signed_data.into_inner().intent_message.value;

    let authenticator_gas_budget = match &move_authenticator {
        Some(_) => env.protocol_config.max_auth_gas(),
        None => 0,
    };

    let prepared = prepare_transaction(env, store, transaction, checks, authenticator_gas_budget)?;
    let mut trace_builder = env.trace_enabled().then(MoveTraceBuilder::new);

    let result = match move_authenticator {
        Some(authenticator) => {
            execute_with_move_authenticator(env, store, prepared, authenticator, &mut trace_builder)
        }
        None => execute_prepared(env, store, prepared, checks, &mut trace_builder),
    }?;
    let artifacts = env.collect_artifacts(trace_builder);
    Ok(DebugSimulateResult { result, artifacts })
}

// ---------------------------------------------------------------------------
// Internal: shared transaction preparation
// ---------------------------------------------------------------------------

/// Validated and resolved transaction data, ready for execution.
struct PreparedTransaction {
    transaction: TransactionData,
    gas_status: IotaGasStatus,
    checked_input_objects: CheckedInputObjects,
    mock_gas_id: Option<ObjectID>,
}

/// Validate a transaction, resolve all objects from the store, and return a
/// [`PreparedTransaction`] ready for execution.
fn prepare_transaction(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    mut transaction: TransactionData,
    checks: VmChecks,
    authenticator_gas_budget: u64,
) -> Result<PreparedTransaction> {
    transaction
        .validity_check_no_gas_check(&env.protocol_config)
        .map_err(|e| {
            crate::error::LocalExecError::validation(
                "transaction validity check",
                anyhow::anyhow!(e),
            )
        })?;

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

    Ok(PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    })
}

/// Execute a prepared transaction via `dev_inspect_transaction` (no
/// authenticator).
fn execute_prepared(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    checks: VmChecks,
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> Result<SimulateTransactionResult> {
    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

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
        trace_builder_opt,
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
/// `AuthenticatorFunctionRefV1` from the account object's dynamic field,
/// and runs both the authenticator function and the transaction through
/// `authenticate_then_execute_transaction_to_effects`.
fn execute_with_move_authenticator(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    authenticator: MoveAuthenticator,
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> Result<SimulateTransactionResult> {
    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

    // Resolve authenticator input objects (separate from transaction inputs).
    let auth_input_object_kinds = authenticator.input_objects();
    let (_, auth_input_objects) = build_input_objects(store, &auth_input_object_kinds)?;

    // Build the union of transaction + authenticator input objects.
    let mut union_inputs = checked_input_objects.into_inner();
    for obj in auth_input_objects.iter() {
        if union_inputs.find_object_id_mut(obj.id()).is_none() {
            union_inputs.push(obj.clone());
        }
    }
    let union_checked = CheckedInputObjects::new_with_checked_transaction_inputs(union_inputs);
    let auth_checked = CheckedInputObjects::new_with_checked_transaction_inputs(auth_input_objects);

    // Load the AuthenticatorFunctionRefV1 from the account object's dynamic field.
    let authenticator_fn_ref = resolve_authenticator_function_ref(store, &authenticator)?;

    // BCS-serialize the transaction data for the auth context.
    let tx_data_bytes =
        bcs::to_bytes(&transaction).expect("TransactionData serialization cannot fail");

    // Execute with authenticator.
    let (kind, signer, gas_data) = transaction.execution_parts();
    let (inner_temp_store, _, effects, execution_result) = env
        .executor
        .authenticate_then_execute_transaction_to_effects(
            store,
            &env.protocol_config,
            env.limits_metrics.clone(),
            false,
            &HashSet::new(),
            &env.epoch_id,
            env.epoch_timestamp_ms,
            gas_data,
            gas_status,
            vec![(authenticator, authenticator_fn_ref, auth_checked)],
            union_checked,
            kind,
            signer,
            transaction.digest(),
            tx_data_bytes,
            trace_builder_opt,
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

/// Load the [`AuthenticatorFunctionRefForExecution`] from the account object's
/// dynamic field in the store.
fn resolve_authenticator_function_ref(
    store: &dyn BackingStore,
    authenticator: &MoveAuthenticator,
) -> Result<AuthenticatorFunctionRefForExecution> {
    let (account_object_id, _version, _digest) = authenticator
        .object_to_authenticate_components()
        .map_err(|e| anyhow::anyhow!("invalid object_to_authenticate: {e}"))?;

    // Derive the dynamic field ID that stores the AuthenticatorFunctionRefV1.
    let field_id = dynamic_field::derive_dynamic_field_id(
        account_object_id,
        &AuthenticatorFunctionRefV1Key::tag().into(),
        &AuthenticatorFunctionRefV1Key::default().to_bcs_bytes(),
    )
    .map_err(|e| anyhow::anyhow!("failed to derive authenticator field ID: {e}"))?;

    // Load the field object from the store.
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
        .try_as_move()
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

// ---------------------------------------------------------------------------
// Shared helpers for executor prefetch logic
// ---------------------------------------------------------------------------

/// Collect all unique object IDs referenced by a transaction (input objects,
/// gas coins, and receiving objects).
pub(crate) fn collect_all_object_ids(transaction: &TransactionData) -> Result<Vec<ObjectID>> {
    let input_object_kinds = transaction.input_objects()?;
    let receiving_object_refs = transaction.receiving_objects();

    let mut ids: Vec<ObjectID> = input_object_kinds
        .iter()
        .map(|kind| kind.object_id())
        .collect();
    for gas_ref in transaction.gas() {
        ids.push(gas_ref.0);
    }
    for objref in &receiving_object_refs {
        ids.push(objref.0);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Split a transaction's object references into two buckets:
///
/// - **versioned**: owned/immutable objects (and gas/receiving objects) with
///   their exact transaction version.
/// - **latest**: shared objects and packages that should be fetched at the
///   latest version.
///
/// Objects that appear in the `latest` set are removed from `versioned`.
pub(crate) fn split_transaction_refs(
    transaction: &TransactionData,
) -> Result<(BTreeMap<ObjectID, SequenceNumber>, Vec<ObjectID>)> {
    let input_object_kinds = transaction.input_objects()?;
    let receiving_object_refs = transaction.receiving_objects();

    let mut versioned: BTreeMap<ObjectID, SequenceNumber> = BTreeMap::new();
    let mut latest: Vec<ObjectID> = Vec::new();

    for kind in &input_object_kinds {
        match kind {
            InputObjectKind::ImmOrOwnedMoveObject(objref) => {
                versioned.insert(objref.0, objref.1);
            }
            InputObjectKind::SharedMoveObject { id, .. } => latest.push(*id),
            InputObjectKind::MovePackage(id) => latest.push(*id),
        }
    }
    for gas_ref in transaction.gas() {
        versioned.entry(gas_ref.0).or_insert(gas_ref.1);
    }
    for objref in &receiving_object_refs {
        versioned.entry(objref.0).or_insert(objref.1);
    }

    // An object shouldn't appear in both sets, but be safe.
    for id in &latest {
        versioned.remove(id);
    }
    latest.sort();
    latest.dedup();

    Ok((versioned, latest))
}
