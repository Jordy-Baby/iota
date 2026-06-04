// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`LocalVm`] — the single decode -> store -> execute -> introspect executor.
//!
//! `LocalVm` owns a [`Store`] and a Move execution engine configured for the
//! chain described by a [`ChainContext`]. Each [`LocalVm::execute`] /
//! [`LocalVm::execute_signed`] call runs a transaction in one of three
//! [`ExecutionMode`]s:
//!
//! - [`ExecutionMode::DevInspect`] — relaxed Move VM checks, no commit.
//! - [`ExecutionMode::DryRun`] — full sign-time checks, no commit.
//! - [`ExecutionMode::Execute`] — full checks; on success the effects (writes
//!   *and* deletions) are applied back into the store and
//!   [`ExecutionResult::committed`] is `true`.
//!
//! `DevInspect`/`DryRun` leave the store untouched.

use std::{collections::HashSet, sync::Arc};

use iota_execution::Executor;
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_sdk_types::{Address as IotaAddress, Identifier, ObjectId, StructTag};
use iota_types::{
    account_abstraction::{
        account::AuthenticatorFunctionRefV1Key,
        authenticator_function::{
            AuthenticatorFunctionRefForExecution, AuthenticatorFunctionRefV1,
        },
    },
    digests::TransactionDigest,
    dynamic_field::{self, Field},
    effects::{TransactionEffects, TransactionEffectsAPI, TransactionEvents},
    event::Event,
    gas::{GasCostSummary, IotaGasStatus},
    gas_coin::NANOS_PER_IOTA,
    layout_resolver::LayoutResolver,
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
    transaction_executor::SimulateTransactionResult,
};
use move_core_types::annotated_value::{MoveDatatypeLayout, MoveTypeLayout, MoveValue};
use move_trace_format::format::MoveTraceBuilder;

use crate::{
    debug::{DebugArtifacts, DebugConfig},
    error::{ExecutionError, ValidationError, VmError, VmSdkError},
    store::{Store, StoreBackend},
};

/// Value the VM stuffs into a mock gas coin when a transaction has no explicit
/// gas payment. One IOTA's worth of NANOs — wide enough to cover any realistic
/// single-tx gas budget.
const MOCK_GAS_COIN_NANOS: u64 = 1_000_000_000 * NANOS_PER_IOTA;

// ---------------------------------------------------------------------------
// Public input / output types
// ---------------------------------------------------------------------------

/// The chain parameters a [`LocalVm`] needs, replacing the four loose scalar
/// arguments the older executors took. This is an input config, so it is
/// **not** `#[non_exhaustive]`.
#[derive(Debug, Clone)]
pub struct ChainContext {
    pub protocol_version: ProtocolVersion,
    pub reference_gas_price: u64,
    pub epoch_id: u64,
    pub epoch_timestamp_ms: u64,
    pub chain: Chain,
}

/// How a transaction is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ExecutionMode {
    /// Relaxed Move VM checks; the store is never modified.
    DevInspect,
    /// Full sign-time checks; the store is never modified. The default: full
    /// validation without committing.
    #[default]
    DryRun,
    /// Full sign-time checks; on success, effects are applied back to the
    /// store and [`ExecutionResult::committed`] is `true`.
    Execute,
}

/// The outcome of signature verification for a run.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignatureStatus {
    /// No signature was supplied (unsigned [`LocalVm::execute`]).
    NotChecked,
    /// Signatures verified successfully. For a [`MoveAuthenticator`] this means
    /// the authenticator function did not abort during execution.
    Verified,
    /// Signature verification failed.
    Failed(crate::error::SignatureError),
}

/// Options for a single run: the [`ExecutionMode`] plus a [`DebugConfig`].
/// Input config — **not** `#[non_exhaustive]`.
#[derive(Debug, Clone, Default)]
pub struct ExecuteOptions {
    pub mode: ExecutionMode,
    pub debug: DebugConfig,
}

impl ExecuteOptions {
    /// Dev-inspect mode (relaxed checks).
    pub fn dev_inspect() -> Self {
        Self {
            mode: ExecutionMode::DevInspect,
            debug: DebugConfig::default(),
        }
    }

    /// Dry-run mode (full checks, no commit).
    pub fn dry_run() -> Self {
        Self {
            mode: ExecutionMode::DryRun,
            debug: DebugConfig::default(),
        }
    }

    /// Execute mode (full checks, commit on success).
    pub fn execute() -> Self {
        Self {
            mode: ExecutionMode::Execute,
            debug: DebugConfig::default(),
        }
    }

    /// Attach a [`DebugConfig`] to capture prints / profile / trace.
    #[must_use]
    pub fn with_debug(mut self, cfg: DebugConfig) -> Self {
        self.debug = cfg;
        self
    }
}

/// The full result of a run: effects, events, per-command results, the input
/// and output object sets, gas accounting, signature status, whether the run
/// was committed to the store, and any captured debug artifacts. Output type —
/// `#[non_exhaustive]`.
#[non_exhaustive]
pub struct ExecutionResult {
    pub effects: TransactionEffects,
    pub events: Option<TransactionEvents>,
    /// Per-PTB-command `(mutable_reference_outputs, return_values)`.
    pub command_results: Vec<iota_types::execution::ExecutionResult>,
    pub input_objects: Vec<Object>,
    pub output_objects: Vec<Object>,
    pub gas_summary: GasCostSummary,
    pub gas_estimate: GasEstimate,
    pub mock_gas_id: Option<ObjectId>,
    pub status: iota_sdk_types::ExecutionStatus,
    pub signature_status: SignatureStatus,
    /// `true` iff [`ExecutionMode::Execute`] ran successfully and the effects
    /// were applied back to the store.
    pub committed: bool,
    pub debug: Option<DebugArtifacts>,
}

/// Convenience summary of a run's gas ledger. Output type —
/// `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct GasEstimate {
    pub computation_cost: u64,
    pub storage_cost: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
    /// `computation_cost + storage_cost - storage_rebate`. The figure a gas
    /// budget needs to cover.
    pub net_gas_usage: i64,
}

impl GasEstimate {
    fn from_summary(s: &GasCostSummary) -> Self {
        Self {
            computation_cost: s.computation_cost,
            storage_cost: s.storage_cost,
            storage_rebate: s.storage_rebate,
            non_refundable_storage_fee: s.non_refundable_storage_fee,
            net_gas_usage: s.net_gas_usage(),
        }
    }

    /// Suggest a gas budget for a future execution of the same transaction:
    /// `net_gas_usage * headroom_factor`, rounded up. Returns `0` for a net
    /// rebate.
    pub fn suggested_budget_with_headroom(&self, headroom_factor: f64) -> u64 {
        if self.net_gas_usage <= 0 {
            return 0;
        }
        ((self.net_gas_usage as f64) * headroom_factor).ceil() as u64
    }
}

/// One decoded Move event with every field named and typed. Output type —
/// `#[non_exhaustive]`.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecodedEvent {
    /// Package that emitted the event.
    pub package_id: ObjectId,
    /// Module inside that package that emitted the event.
    pub module: Identifier,
    /// The event struct's name.
    pub name: Identifier,
    /// Address of the transaction sender.
    pub sender: IotaAddress,
    /// The event struct's type, e.g. `0x2::coin::CoinEvent`.
    pub type_tag: StructTag,
    /// Decoded event payload.
    pub value: MoveValue,
}

// ---------------------------------------------------------------------------
// LocalVm
// ---------------------------------------------------------------------------

/// The local Move VM executor. Owns a [`Store`] and the execution engine for a
/// single [`ChainContext`].
pub struct LocalVm {
    protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
    store: Box<dyn Store>,
}

impl LocalVm {
    /// Build a `LocalVm` for the given chain context, taking ownership of the
    /// store.
    pub fn new(ctx: ChainContext, store: impl Store + 'static) -> Result<Self, VmSdkError> {
        let protocol_config = ProtocolConfig::get_for_version(ctx.protocol_version, ctx.chain);
        Ok(Self {
            protocol_config,
            reference_gas_price: ctx.reference_gas_price,
            epoch_id: ctx.epoch_id,
            epoch_timestamp_ms: ctx.epoch_timestamp_ms,
            limits_metrics: Arc::new(new_limits_metrics()),
            bytecode_verifier_metrics: Arc::new(new_bytecode_verifier_metrics()),
            store: Box::new(store),
        })
    }

    /// A mutable reference to the underlying store, for inserting objects
    /// before a run.
    pub fn store_mut(&mut self) -> &mut dyn Store {
        self.store.as_mut()
    }

    /// Run an unsigned transaction.
    pub fn execute(
        &mut self,
        tx: TransactionData,
        opts: ExecuteOptions,
    ) -> Result<ExecutionResult, VmSdkError> {
        let env = ExecutionEnv::new(self, &opts.debug)?;
        let prepared = {
            let backend = StoreBackend::new(self.store.as_ref());
            prepare_transaction(&env, &backend, tx, opts.mode, 0)?
        };
        // The plain dev_inspect path does not accept a trace builder (only the
        // authenticator path does), so no trace is captured here.
        let trace_builder = env.trace_enabled().then(MoveTraceBuilder::new);
        let sim = {
            let backend = StoreBackend::new(self.store.as_ref());
            execute_prepared(&env, &backend, prepared, opts.mode)?
        };
        let artifacts = env.collect_artifacts(trace_builder);
        self.finish(sim, opts.mode, SignatureStatus::NotChecked, artifacts)
    }

    /// Run a signed transaction, verifying signatures first.
    ///
    /// Standard schemes are verified cryptographically before execution; a
    /// [`MoveAuthenticator`] is verified by running its function inside the VM
    /// during execution.
    pub fn execute_signed(
        &mut self,
        signed: SenderSignedData,
        opts: ExecuteOptions,
    ) -> Result<ExecutionResult, VmSdkError> {
        let env = ExecutionEnv::new(self, &opts.debug)?;

        let verify_params = VerifyParams::default();
        verify_sender_signed_data_message_signatures(&signed, &verify_params)
            .map_err(crate::error::SignatureError::new)?;

        let move_authenticator = signed.sender_move_authenticator().cloned();
        // The auth digests must be computed from the signed data before it is
        // consumed; the `MoveAuthenticator` execution path needs them in its
        // `AuthContextData`.
        let auth_digests = signed
            .compute_auth_digests()
            .map_err(crate::error::SignatureError::new)?;
        let transaction = signed.into_inner().intent_message.value;

        let authenticator_gas_budget = match &move_authenticator {
            Some(_) => self.protocol_config.max_auth_gas(),
            None => 0,
        };

        let prepared = {
            let backend = StoreBackend::new(self.store.as_ref());
            prepare_transaction(
                &env,
                &backend,
                transaction,
                opts.mode,
                authenticator_gas_budget,
            )?
        };
        let mut trace_builder = env.trace_enabled().then(MoveTraceBuilder::new);

        let sim = {
            let backend = StoreBackend::new(self.store.as_ref());
            match move_authenticator {
                Some(authenticator) => execute_with_move_authenticator(
                    &env,
                    &backend,
                    prepared,
                    authenticator,
                    auth_digests,
                    &mut trace_builder,
                )?,
                None => execute_prepared(&env, &backend, prepared, opts.mode)?,
            }
        };
        let artifacts = env.collect_artifacts(trace_builder);

        // Signatures cleared verification above. For a `MoveAuthenticator`, the
        // authenticator function's verdict is the run's overall status, so a
        // successful run means the authenticator accepted.
        let signature_status = if sim.effects.status().is_success() {
            SignatureStatus::Verified
        } else {
            SignatureStatus::Failed(crate::error::SignatureError::new(
                "authenticator function rejected the signature",
            ))
        };
        self.finish(sim, opts.mode, signature_status, artifacts)
    }

    /// Decode a [`TransactionEvents`] payload into fully-annotated
    /// [`DecodedEvent`]s using this VM's type-layout resolver and the store.
    /// One `Result` per event so a single bad event doesn't mask the rest.
    pub fn decode_events(
        &self,
        events: &TransactionEvents,
    ) -> Vec<Result<DecodedEvent, VmSdkError>> {
        // Build a default-config executor purely for its layout resolver.
        let executor = match build_executor(&self.protocol_config, &DebugConfig::default()) {
            Ok(e) => e,
            Err(e) => return vec![Err(e)],
        };
        let backend = StoreBackend::new(self.store.as_ref());
        let mut resolver = executor.type_layout_resolver(Box::new(&backend));
        events
            .0
            .iter()
            .map(|ev| decode_one_event(ev, resolver.as_mut()))
            .collect()
    }

    /// Assemble an [`ExecutionResult`] from a raw simulation, committing the
    /// effects to the store when the mode is [`ExecutionMode::Execute`] and the
    /// run succeeded.
    fn finish(
        &mut self,
        sim: SimulateTransactionResult,
        mode: ExecutionMode,
        signature_status: SignatureStatus,
        artifacts: Option<DebugArtifacts>,
    ) -> Result<ExecutionResult, VmSdkError> {
        let gas_summary = sim.effects.gas_cost_summary().clone();
        let gas_estimate = GasEstimate::from_summary(&gas_summary);
        let status = sim.effects.status().clone();

        let succeeded = sim.effects.status().is_success();
        let committed = matches!(mode, ExecutionMode::Execute) && succeeded;
        if committed {
            self.apply_effects(&sim);
        }

        Ok(ExecutionResult {
            effects: sim.effects,
            events: sim.events,
            command_results: sim.execution_result.unwrap_or_default(),
            input_objects: sim.input_objects.into_values().collect(),
            output_objects: sim.output_objects.into_values().collect(),
            gas_summary,
            gas_estimate,
            mock_gas_id: sim.mock_gas_id,
            status,
            signature_status,
            committed,
            debug: artifacts,
        })
    }

    /// Apply created/mutated/deleted/wrapped changes back into the store so a
    /// subsequent run sees them.
    fn apply_effects(&mut self, sim: &SimulateTransactionResult) {
        // Write created/mutated/unwrapped objects.
        for obj in sim.output_objects.values() {
            self.store.insert(obj.clone());
        }
        // Drop deleted and wrapped objects.
        for objref in sim.effects.deleted() {
            self.store.remove(&objref.object_id);
        }
        for objref in sim.effects.wrapped() {
            self.store.remove(&objref.object_id);
        }
        // The one-shot mock gas coin is never persisted.
        if let Some(id) = sim.mock_gas_id {
            self.store.remove(&id);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal execution environment (per-run, holds the debug-configured engine)
// ---------------------------------------------------------------------------

/// Per-run engine + debug wiring, built fresh for each `execute*` call because
/// `iota_execution::executor` bakes the `silent` flag and profiler path in at
/// construction.
struct ExecutionEnv {
    protocol_config: ProtocolConfig,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    executor: Arc<dyn Executor + Send + Sync>,
    limits_metrics: Arc<LimitsMetrics>,
    bytecode_verifier_metrics: Arc<BytecodeVerifierMetrics>,
    debug: DebugConfig,
    #[cfg(not(target_arch = "wasm32"))]
    capture_profile_dir: Option<std::path::PathBuf>,
}

impl ExecutionEnv {
    fn new(vm: &LocalVm, debug: &DebugConfig) -> Result<Self, VmSdkError> {
        #[cfg(not(target_arch = "wasm32"))]
        let (executor, capture_profile_dir) =
            build_executor_with_profile(&vm.protocol_config, debug)?;
        #[cfg(target_arch = "wasm32")]
        let executor = build_executor(&vm.protocol_config, debug)?;

        Ok(Self {
            protocol_config: vm.protocol_config.clone(),
            reference_gas_price: vm.reference_gas_price,
            epoch_id: vm.epoch_id,
            epoch_timestamp_ms: vm.epoch_timestamp_ms,
            executor,
            limits_metrics: vm.limits_metrics.clone(),
            bytecode_verifier_metrics: vm.bytecode_verifier_metrics.clone(),
            debug: debug.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            capture_profile_dir,
        })
    }

    fn trace_enabled(&self) -> bool {
        self.debug.trace
    }

    /// Materialise captured artifacts: the gas profile (native only) and the
    /// finished trace. Returns `None` when no debug capture was requested.
    fn collect_artifacts(&self, trace_builder: Option<MoveTraceBuilder>) -> Option<DebugArtifacts> {
        if !self.debug.any_enabled() {
            return None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let profile = collect_profile(&self.debug, self.capture_profile_dir.as_deref());
        #[cfg(target_arch = "wasm32")]
        let profile = None;

        Some(DebugArtifacts {
            // The in-memory `debug::print` sink is not available in this build;
            // with `capture_debug_prints` set, prints are forwarded to stdout
            // by the (non-silent) executor instead.
            prints: Vec::new(),
            profile,
            trace: trace_builder.map(|b| b.into_trace()),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for ExecutionEnv {
    fn drop(&mut self) {
        if let Some(dir) = self.capture_profile_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Build a Move executor with the `silent` flag derived from
/// [`DebugConfig::capture_debug_prints`] and no profiler.
fn build_executor(
    protocol_config: &ProtocolConfig,
    debug: &DebugConfig,
) -> Result<Arc<dyn Executor + Send + Sync>, VmSdkError> {
    let silent = !debug.capture_debug_prints;
    iota_execution::executor(protocol_config, silent, None).map_err(|e| VmError::new(e).into())
}

// ---------------------------------------------------------------------------
// Native-only: gas-profile capture (uses std::fs + serde_json)
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
fn build_executor_with_profile(
    protocol_config: &ProtocolConfig,
    debug: &DebugConfig,
) -> Result<(Arc<dyn Executor + Send + Sync>, Option<std::path::PathBuf>), VmSdkError> {
    use crate::debug::ProfileSink;

    let (profile_path, capture_dir) = match &debug.profile {
        Some(ProfileSink::Path(p)) => (Some(p.clone()), None),
        Some(ProfileSink::Capture) => {
            let dir = profile_capture_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|e| VmError::new(format!("create profile dir: {e}")))?;
            (Some(dir.join("profile.json")), Some(dir))
        }
        None => (None, None),
    };

    let silent = !debug.capture_debug_prints;
    let executor =
        iota_execution::executor(protocol_config, silent, profile_path).map_err(VmError::new)?;
    Ok((executor, capture_dir))
}

#[cfg(not(target_arch = "wasm32"))]
fn collect_profile(
    debug: &DebugConfig,
    capture_dir: Option<&std::path::Path>,
) -> Option<crate::debug::ProfileOutput> {
    use crate::debug::{ProfileOutput, ProfileSink};
    match (&debug.profile, capture_dir) {
        (Some(ProfileSink::Path(p)), _) => Some(ProfileOutput::Path(p.clone())),
        (Some(ProfileSink::Capture), Some(dir)) => merge_profile_dir(dir),
        _ => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn profile_capture_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("iota-vm-sdk-gas-profile-{pid}-{n}"))
}

/// Merge every Speedscope JSON file the profiler wrote into `dir` into one
/// document. The profiler writes one file per VM invocation (e.g. an
/// authenticator call + the PTB body), each with its own frames table; the
/// merge concatenates `profiles` and rebuilds a de-duplicated `shared.frames`.
#[cfg(not(target_arch = "wasm32"))]
fn merge_profile_dir(dir: &std::path::Path) -> Option<crate::debug::ProfileOutput> {
    use crate::debug::ProfileOutput;
    let entries = std::fs::read_dir(dir).ok()?;
    let mut docs: Vec<serde_json::Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                docs.push(doc);
            }
        }
    }
    if docs.is_empty() {
        return None;
    }
    if docs.len() == 1 {
        return serde_json::to_vec(&docs[0]).ok().map(ProfileOutput::Json);
    }
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
        "exporter": "iota-vm-sdk (merged)",
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "shared": { "frames": merged_frames },
        "profiles": merged_profiles,
    });
    serde_json::to_vec(&merged).ok().map(ProfileOutput::Json)
}

// ---------------------------------------------------------------------------
// Target-gated metrics constructors
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Shared transaction preparation + execution
// ---------------------------------------------------------------------------

struct PreparedTransaction {
    transaction: TransactionData,
    gas_status: IotaGasStatus,
    checked_input_objects: CheckedInputObjects,
    mock_gas_id: Option<ObjectId>,
}

fn prepare_transaction(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    mut transaction: TransactionData,
    mode: ExecutionMode,
    authenticator_gas_budget: u64,
) -> Result<PreparedTransaction, VmSdkError> {
    transaction
        .validity_check_no_gas_check(&env.protocol_config)
        .map_err(|e| ValidationError::new("transaction validity check", e))?;

    // Update gas payment references to match actual object versions in the store.
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

    let raw_input_object_kinds = transaction
        .input_objects()
        .map_err(|e| ValidationError::new("collect input objects", e))?;
    let receiving_object_refs = transaction.receiving_objects();

    let (input_object_kinds, mut input_objects) =
        build_input_objects(store, &raw_input_object_kinds)?;
    let receiving_objects = build_receiving_objects(store, &receiving_object_refs)?;

    // Mint a one-shot mock gas coin if the transaction carries no gas payment.
    let mock_gas_id = if transaction.gas().is_empty() {
        let mock_gas_object = Object::new_move(
            MoveObject::new_gas_coin(1.into(), ObjectId::MAX, MOCK_GAS_COIN_NANOS),
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

    // Deny-list check.
    let deny_config = iota_config::transaction_deny_config::TransactionDenyConfig::default();
    let receiving_object_refs = transaction.receiving_objects();
    iota_transaction_checks::deny::check_transaction_for_signing(
        &transaction,
        &[],
        &input_object_kinds,
        &receiving_object_refs,
        &deny_config,
        store,
    )
    .map_err(|e| ValidationError::new("deny-list check", e))?;

    let (gas_status, checked_input_objects) = if matches!(mode, ExecutionMode::DevInspect) {
        let checked_input_objects = iota_transaction_checks::check_dev_inspect_input(
            &env.protocol_config,
            transaction.kind(),
            input_objects,
            receiving_objects,
        )
        .map_err(|e| ValidationError::new("dev-inspect input check", e))?;
        let gas_status = IotaGasStatus::new(
            transaction.gas_budget(),
            transaction.gas_price(),
            env.reference_gas_price,
            &env.protocol_config,
        )
        .map_err(|e| ValidationError::new("gas status", e))?;
        (gas_status, checked_input_objects)
    } else {
        let verifier_signing_config =
            iota_config::verifier_signing_config::VerifierSigningConfig::default();
        iota_transaction_checks::check_transaction_input(
            &env.protocol_config,
            env.reference_gas_price,
            &transaction,
            input_objects,
            &receiving_objects,
            &env.bytecode_verifier_metrics,
            &verifier_signing_config,
            authenticator_gas_budget,
        )
        .map_err(|e| ValidationError::new("transaction input check", e))?
    };

    Ok(PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    })
}

fn execute_prepared(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    mode: ExecutionMode,
) -> Result<SimulateTransactionResult, VmSdkError> {
    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

    let dev_inspect = matches!(mode, ExecutionMode::DevInspect);
    let (kind, signer, gas_data) = transaction.execution_parts();
    // The current `dev_inspect_transaction` engine entry point does not accept a
    // `MoveTraceBuilder`; instruction tracing is only available on the
    // authenticator path (`authenticate_then_execute_transaction_to_effects`).
    let (inner_temp_store, _, effects, execution_result) = env.executor.dev_inspect_transaction(
        store,
        &env.protocol_config,
        env.limits_metrics.clone(),
        false,
        &HashSet::new(),
        &env.epoch_id,
        env.epoch_timestamp_ms,
        checked_input_objects,
        gas_data,
        gas_status,
        kind,
        signer,
        transaction.digest(),
        dev_inspect,
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

fn execute_with_move_authenticator(
    env: &ExecutionEnv,
    store: &dyn BackingStore,
    prepared: PreparedTransaction,
    authenticator: MoveAuthenticator,
    auth_digests: (
        iota_types::digests::Digest,
        Option<iota_types::digests::Digest>,
    ),
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> Result<SimulateTransactionResult, VmSdkError> {
    use iota_types::{
        account_abstraction::authenticator_function::extract_auth_fun_refs,
        auth_context::AuthContextData,
    };

    let PreparedTransaction {
        transaction,
        gas_status,
        checked_input_objects,
        mock_gas_id,
    } = prepared;

    // Resolve the authenticator's input objects (separate from tx inputs).
    let auth_input_object_kinds = authenticator.input_objects();
    let (_, auth_input_objects) = build_input_objects(store, &auth_input_object_kinds)?;

    // Union of transaction + authenticator inputs for the main execution.
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

    // Build the auth context: map the authenticator's address to its function
    // ref for the sender (and sponsor, if sponsored).
    let (sender_auth_digest, sponsor_auth_digest) = auth_digests;
    let authenticator_address = authenticator.address().ok();
    let (sender_authenticator_function_ref, sponsor_authenticator_function_ref) =
        extract_auth_fun_refs(signer, gas_data.owner, |address| {
            if authenticator_address == Some(address) {
                Some(authenticator_fn_ref.authenticator_function_ref.clone())
            } else {
                None
            }
        });
    let auth_context_data = AuthContextData {
        transaction_data_bytes: tx_data_bytes,
        sender_auth_digest,
        sponsor_auth_digest,
        sender_authenticator_function_ref,
        sponsor_authenticator_function_ref,
    };

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
            auth_context_data,
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
) -> Result<AuthenticatorFunctionRefForExecution, VmSdkError> {
    let (account_object_id, _version, _digest) = authenticator
        .object_to_authenticate_components()
        .map_err(|e| ValidationError::new("invalid object_to_authenticate", e))?;

    let field_id = dynamic_field::derive_dynamic_field_id(
        account_object_id,
        &AuthenticatorFunctionRefV1Key::tag().into(),
        &AuthenticatorFunctionRefV1Key::default().to_bcs_bytes(),
    )
    .map_err(|e| ValidationError::new("derive authenticator field id", e))?;

    let field_obj =
        store
            .as_object_store()
            .get_object(&field_id)
            .ok_or(VmSdkError::MissingObject {
                id: field_id,
                version: None,
            })?;

    let field_move_object = field_obj.data.as_struct_opt().ok_or_else(|| {
        ValidationError::new(
            "authenticator dynamic field",
            "field object is not a Move object",
        )
    })?;

    let field: Field<AuthenticatorFunctionRefV1Key, AuthenticatorFunctionRefV1> = field_move_object
        .to_rust()
        .map_err(|e| ValidationError::new("deserialize AuthenticatorFunctionRefV1", e))?;

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
) -> Result<(Vec<InputObjectKind>, InputObjects), VmSdkError> {
    let mut updated_kinds = Vec::new();
    let mut input_objects = Vec::new();
    for kind in input_object_kinds {
        let obj = store
            .as_object_store()
            .get_object(&kind.object_id())
            .ok_or(VmSdkError::MissingObject {
                id: kind.object_id(),
                version: None,
            })?;

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

fn build_receiving_objects(
    store: &dyn BackingStore,
    receiving_object_refs: &[iota_types::base_types::ObjectRef],
) -> Result<ReceivingObjects, VmSdkError> {
    let mut receiving_objects = Vec::new();
    for objref in receiving_object_refs {
        let obj = store
            .as_object_store()
            .get_object(&objref.object_id)
            .ok_or(VmSdkError::MissingObject {
                id: objref.object_id,
                version: Some(objref.version),
            })?;
        let updated_ref = obj.compute_object_reference();
        receiving_objects.push(ReceivingObjectReadResult::new(updated_ref, obj.into()));
    }
    Ok(receiving_objects.into())
}

/// Decode a single event against the resolver.
fn decode_one_event(
    event: &Event,
    resolver: &mut dyn LayoutResolver,
) -> Result<DecodedEvent, VmSdkError> {
    let layout = resolver
        .get_annotated_layout(&event.type_)
        .map_err(|e| ExecutionError::new(format!("resolve layout for {}: {e}", event.type_)))?;
    let value = match layout {
        MoveDatatypeLayout::Struct(s) => {
            MoveValue::simple_deserialize(&event.contents, &MoveTypeLayout::Struct(s))
        }
        MoveDatatypeLayout::Enum(e_layout) => {
            MoveValue::simple_deserialize(&event.contents, &MoveTypeLayout::Enum(e_layout))
        }
    }
    .map_err(|e| ExecutionError::new(format!("bcs deserialize {}: {e}", event.type_)))?;

    Ok(DecodedEvent {
        package_id: event.package_id,
        module: event.module.clone(),
        name: event.type_.name().clone(),
        sender: event.sender,
        type_tag: event.type_.clone(),
        value,
    })
}
