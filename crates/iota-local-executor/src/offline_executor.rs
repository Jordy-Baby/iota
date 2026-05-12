// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`OfflineExecutor`] — fully offline, all objects provided upfront.

use anyhow::Result;
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    effects::TransactionEvents,
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    DebugConfig, DebugSimulateResult, InMemoryStore,
    events::DecodedEvent,
    execution::{self, ExecutionEnv},
};

/// A fully offline executor that runs Move VM transactions using only
/// pre-provided objects. No network access is performed.
///
/// # Example
///
/// ```rust,no_run
/// use iota_local_executor::{InMemoryStore, OfflineExecutor};
/// use iota_protocol_config::ProtocolVersion;
///
/// // `with_framework()` pre-loads the built-in framework packages so Move
/// // calls can resolve. Add any caller-supplied objects with `insert()`.
/// let mut store = InMemoryStore::with_framework();
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
    pub fn new(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        store: InMemoryStore,
    ) -> Result<Self> {
        Self::with_debug(
            protocol_version,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            store,
            DebugConfig::default(),
        )
    }

    /// Create a new fully offline executor with a custom [`DebugConfig`] that
    /// enables debug-print capture, gas profiling, and/or execution tracing.
    pub fn with_debug(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        store: InMemoryStore,
        debug_config: DebugConfig,
    ) -> Result<Self> {
        let env = ExecutionEnv::with_debug(
            protocol_version,
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            debug_config,
        )?;
        Ok(Self { env, store })
    }

    /// Get a mutable reference to the underlying store, allowing insertion of
    /// additional objects before execution.
    pub fn store_mut(&mut self) -> &mut InMemoryStore {
        &mut self.store
    }

    /// Decode a `TransactionEvents` payload into fully-annotated
    /// [`DecodedEvent`]s using this executor's Move VM type-layout resolver
    /// and package store. One `Result` per event so a single bad event
    /// doesn't mask the rest.
    pub fn decode_events(&self, events: &TransactionEvents) -> Vec<Result<DecodedEvent>> {
        execution::decode_events_with(&self.env, &self.store, events)
    }

    /// Simulate a transaction offline.
    ///
    /// - `VmChecks::Enabled` → dry-run mode (full checks, like a real
    ///   transaction).
    /// - `VmChecks::Disabled` → dev-inspect mode (relaxed Move VM checks).
    pub fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        execution::simulate(&self.env, &self.store, transaction, checks)
    }

    /// Simulate a transaction and return the captured [`DebugArtifacts`]
    /// alongside the result. With a default [`DebugConfig`] the artifacts are
    /// empty and this is equivalent to [`Self::simulate_transaction`].
    pub fn simulate_transaction_with_debug(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        execution::simulate_with_debug(&self.env, &self.store, transaction, checks)
    }

    /// Simulate a **signed** transaction offline, verifying signatures first.
    ///
    /// For standard schemes (Ed25519, Secp256k1, Secp256r1, MultiSig) the
    /// full cryptographic check runs before execution. For
    /// `MoveAuthenticator` signatures the sender address is checked upfront,
    /// then the authenticator function is executed inside the Move VM — this
    /// is the only way to fully validate `MoveAuthenticator` signatures.
    pub fn simulate_signed_transaction(
        &self,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        execution::simulate_signed(&self.env, &self.store, signed_data, checks)
    }

    /// Signed-transaction variant of [`Self::simulate_transaction_with_debug`].
    pub fn simulate_signed_transaction_with_debug(
        &self,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        execution::simulate_signed_with_debug(&self.env, &self.store, signed_data, checks)
    }
}
