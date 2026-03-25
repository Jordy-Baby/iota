// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`OfflineExecutor`] — fully offline, all objects provided upfront.

use anyhow::Result;
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    InMemoryStore,
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
    pub fn new(
        protocol_version: ProtocolVersion,
        reference_gas_price: u64,
        epoch_id: u64,
        epoch_timestamp_ms: u64,
        store: InMemoryStore,
    ) -> Result<Self> {
        let env = ExecutionEnv::new(
            protocol_version,
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
}
