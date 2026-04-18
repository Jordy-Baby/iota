// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`GrpcExecutor`] — fetches objects from a remote node via gRPC and executes
//! locally.

use anyhow::Result;
use iota_grpc_client::Client as GrpcClient;
use iota_sdk_types::ObjectId;
use iota_types::{
    object::Object,
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    ChainInfo, DebugConfig, DebugSimulateResult,
    caching_store::{GrpcFetcher, GrpcStore},
    execution::{self, ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    json_rpc_executor::ObjectFetchMode,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node via gRPC.
///
/// The executor owns a persistent [`GrpcStore`] that caches fetched objects
/// across simulation calls. See [`crate::JsonRpcExecutor`] for the caching
/// model.
pub struct GrpcExecutor {
    client: GrpcClient,
    env: ExecutionEnv,
    fetch_mode: ObjectFetchMode,
    shared_store: GrpcStore,
}

impl GrpcExecutor {
    /// Create a new `GrpcExecutor` connected to the given gRPC endpoint.
    pub async fn new(client: GrpcClient) -> Result<Self> {
        Self::with_debug(client, DebugConfig::default()).await
    }

    /// Create a new `GrpcExecutor` with a custom [`DebugConfig`] for
    /// debug-print capture, gas profiling, and/or execution tracing.
    pub async fn with_debug(client: GrpcClient, debug_config: DebugConfig) -> Result<Self> {
        let info = ChainInfo::fetch_from_grpc(&client).await?;
        Self::with_chain_info_and_debug(client, info, debug_config)
    }

    /// Create a new `GrpcExecutor` using a pre-fetched [`ChainInfo`]. See
    /// [`crate::JsonRpcExecutor::with_chain_info`].
    pub fn with_chain_info(client: GrpcClient, info: ChainInfo) -> Result<Self> {
        Self::with_chain_info_and_debug(client, info, DebugConfig::default())
    }

    /// [`Self::with_chain_info`] + a caller-supplied [`DebugConfig`].
    pub fn with_chain_info_and_debug(
        client: GrpcClient,
        info: ChainInfo,
        debug_config: DebugConfig,
    ) -> Result<Self> {
        let env = ExecutionEnv::with_debug(
            info.protocol_version,
            info.reference_gas_price,
            info.epoch_id,
            info.epoch_timestamp_ms,
            debug_config,
        )?;

        let shared_store = GrpcStore::new(GrpcFetcher(client.clone()));
        Ok(Self {
            client,
            env,
            fetch_mode: ObjectFetchMode::default(),
            shared_store,
        })
    }

    /// Set the object fetch mode.
    pub fn set_fetch_mode(&mut self, mode: ObjectFetchMode) {
        self.fetch_mode = mode;
    }

    /// Builder-style setter for the object fetch mode.
    pub fn with_fetch_mode(mut self, mode: ObjectFetchMode) -> Self {
        self.fetch_mode = mode;
        self
    }

    /// Simulate a transaction locally against the executor's persistent
    /// [`GrpcStore`] cache. See
    /// [`crate::JsonRpcExecutor::simulate_transaction`] for details on
    /// `VmChecks`, caching, and shared-object limitations.
    pub async fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        self.prefetch_objects(&self.shared_store, &transaction)
            .await?;
        execution::simulate(&self.env, &self.shared_store, transaction, checks)
    }

    /// Reference to the executor's persistent store.
    pub fn store(&self) -> &GrpcStore {
        &self.shared_store
    }

    /// Create a fresh [`GrpcStore`] sharing this executor's gRPC client —
    /// isolated from the persistent cache. Pair with
    /// [`Self::simulate_transaction_with_debug_using`] for one-off runs.
    pub fn new_store(&self) -> GrpcStore {
        GrpcStore::new(GrpcFetcher(self.client.clone()))
    }

    /// Drop all cached objects. The next simulation will fetch from the
    /// remote again.
    pub fn clear_cache(&mut self) {
        self.shared_store = GrpcStore::new(GrpcFetcher(self.client.clone()));
    }

    /// Decode a `TransactionEvents` payload into fully-annotated
    /// [`DecodedEvent`](crate::DecodedEvent)s using this executor's Move VM
    /// type-layout resolver and persistent object cache.
    pub fn decode_events(
        &self,
        events: &iota_types::effects::TransactionEvents,
    ) -> Vec<Result<crate::DecodedEvent>> {
        let mut resolver = self.env.type_layout_resolver(Box::new(&self.shared_store));
        crate::decode_events(events, resolver.as_mut())
    }

    /// Simulate a transaction and return the captured [`DebugArtifacts`]
    /// alongside the result.
    pub async fn simulate_transaction_with_debug(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&self.shared_store, &transaction)
            .await?;
        execution::simulate_with_debug(&self.env, &self.shared_store, transaction, checks)
    }

    /// Variant of [`Self::simulate_transaction_with_debug`] that reuses a
    /// caller-supplied store.
    pub async fn simulate_transaction_with_debug_using(
        &self,
        store: GrpcStore,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&store, &transaction).await?;
        execution::simulate_with_debug(&self.env, &store, transaction, checks)
    }

    /// Simulate a **signed** transaction locally, verifying signatures first.
    ///
    /// For standard schemes (Ed25519, Secp256k1, Secp256r1, MultiSig) the
    /// full cryptographic check runs before execution. For
    /// `MoveAuthenticator` signatures the sender address is checked upfront,
    /// then the authenticator function is executed inside the Move VM — this
    /// is the only way to fully validate `MoveAuthenticator` signatures.
    pub async fn simulate_signed_transaction(
        &self,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        self.prefetch_objects(&self.shared_store, signed_data.transaction_data())
            .await?;
        execution::simulate_signed(&self.env, &self.shared_store, signed_data, checks)
    }

    /// Signed-transaction variant of [`Self::simulate_transaction_with_debug`].
    pub async fn simulate_signed_transaction_with_debug(
        &self,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&self.shared_store, signed_data.transaction_data())
            .await?;
        execution::simulate_signed_with_debug(&self.env, &self.shared_store, signed_data, checks)
    }

    /// Variant of [`Self::simulate_signed_transaction_with_debug`] that reuses
    /// a caller-supplied store.
    pub async fn simulate_signed_transaction_with_debug_using(
        &self,
        store: GrpcStore,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&store, signed_data.transaction_data())
            .await?;
        execution::simulate_signed_with_debug(&self.env, &store, signed_data, checks)
    }

    /// Fetch all objects referenced by the transaction via gRPC.
    async fn prefetch_objects(
        &self,
        store: &GrpcStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        match self.fetch_mode {
            ObjectFetchMode::UseLatestVersions => self.fetch_all_latest(store, transaction).await,
            ObjectFetchMode::UseTransactionVersions => {
                self.fetch_versioned(store, transaction).await
            }
        }
    }

    /// Fetch all objects at their latest version via gRPC.
    async fn fetch_all_latest(
        &self,
        store: &GrpcStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let refs = collect_all_object_ids(transaction)?;
        if refs.is_empty() {
            return Ok(());
        }

        let sdk_refs: Vec<(ObjectId, Option<u64>)> =
            refs.into_iter().map(|id| (id.into(), None)).collect();

        self.fetch_and_insert(store, &sdk_refs).await
    }

    /// Fetch owned/immutable objects at their exact transaction version; fetch
    /// shared objects and packages at the latest version.
    async fn fetch_versioned(
        &self,
        store: &GrpcStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let (versioned, latest) = split_transaction_refs(transaction)?;

        // Fetch versioned objects at their exact versions.
        if !versioned.is_empty() {
            let sdk_refs: Vec<(ObjectId, Option<u64>)> = versioned
                .into_iter()
                .map(|(id, version)| (id.into(), Some(version.value())))
                .collect();
            self.fetch_and_insert(store, &sdk_refs).await?;
        }

        // Fetch shared objects and packages at the latest version.
        if !latest.is_empty() {
            let sdk_refs: Vec<(ObjectId, Option<u64>)> =
                latest.into_iter().map(|id| (id.into(), None)).collect();
            self.fetch_and_insert(store, &sdk_refs).await?;
        }

        Ok(())
    }

    /// Fetch objects from the gRPC node and insert them into the store.
    async fn fetch_and_insert(
        &self,
        store: &GrpcStore,
        refs: &[(ObjectId, Option<u64>)],
    ) -> Result<()> {
        let proto_objects = self
            .client
            .get_objects(refs, None)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch objects via gRPC: {e}"))?
            .into_inner();

        for proto_obj in proto_objects {
            let sdk_obj = proto_obj
                .object()
                .map_err(|e| anyhow::anyhow!("failed to deserialize gRPC object: {e}"))?;
            let obj: Object = sdk_obj
                .try_into()
                .map_err(|e| anyhow::anyhow!("failed to convert object: {e}"))?;
            store.insert(obj);
        }

        Ok(())
    }
}
