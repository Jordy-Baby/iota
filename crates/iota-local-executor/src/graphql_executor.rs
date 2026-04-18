// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`GraphqlExecutor`] — fetches objects from a remote node via GraphQL and
//! executes locally.

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iota_graphql_rpc_client::simple_client::SimpleClient;
use iota_types::{
    object::Object,
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    ChainInfo, DebugConfig, DebugSimulateResult,
    caching_store::{GraphqlFetcher, GraphqlStore},
    execution::{self, ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    json_rpc_executor::ObjectFetchMode,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node via GraphQL.
///
/// The executor owns a persistent [`GraphqlStore`] that caches fetched
/// objects across simulation calls. See [`crate::JsonRpcExecutor`] for the
/// caching model.
pub struct GraphqlExecutor {
    client: SimpleClient,
    env: ExecutionEnv,
    fetch_mode: ObjectFetchMode,
    shared_store: GraphqlStore,
}

impl GraphqlExecutor {
    /// Create a new `GraphqlExecutor` connected to the given GraphQL endpoint.
    pub async fn new(client: SimpleClient) -> Result<Self> {
        Self::with_debug(client, DebugConfig::default()).await
    }

    /// Create a new `GraphqlExecutor` with a custom [`DebugConfig`] for
    /// debug-print capture, gas profiling, and/or execution tracing.
    pub async fn with_debug(client: SimpleClient, debug_config: DebugConfig) -> Result<Self> {
        let info = ChainInfo::fetch_from_graphql(&client).await?;
        Self::with_chain_info_and_debug(client, info, debug_config)
    }

    /// Create a new `GraphqlExecutor` using a pre-fetched [`ChainInfo`]. See
    /// [`crate::JsonRpcExecutor::with_chain_info`].
    pub fn with_chain_info(client: SimpleClient, info: ChainInfo) -> Result<Self> {
        Self::with_chain_info_and_debug(client, info, DebugConfig::default())
    }

    /// [`Self::with_chain_info`] + a caller-supplied [`DebugConfig`].
    pub fn with_chain_info_and_debug(
        client: SimpleClient,
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

        let shared_store = GraphqlStore::new(GraphqlFetcher(client.clone()));
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
    /// [`GraphqlStore`] cache. See
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
    pub fn store(&self) -> &GraphqlStore {
        &self.shared_store
    }

    /// Create a fresh [`GraphqlStore`] sharing this executor's GraphQL
    /// client — isolated from the persistent cache.
    pub fn new_store(&self) -> GraphqlStore {
        GraphqlStore::new(GraphqlFetcher(self.client.clone()))
    }

    /// Drop all cached objects. The next simulation will fetch from the
    /// remote again.
    pub fn clear_cache(&mut self) {
        self.shared_store = GraphqlStore::new(GraphqlFetcher(self.client.clone()));
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
        store: GraphqlStore,
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
        store: GraphqlStore,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&store, signed_data.transaction_data())
            .await?;
        execution::simulate_signed_with_debug(&self.env, &store, signed_data, checks)
    }

    /// Fetch all objects referenced by the transaction via GraphQL.
    async fn prefetch_objects(
        &self,
        store: &GraphqlStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        match self.fetch_mode {
            ObjectFetchMode::UseLatestVersions => self.fetch_all_latest(store, transaction).await,
            ObjectFetchMode::UseTransactionVersions => {
                self.fetch_versioned(store, transaction).await
            }
        }
    }

    /// Fetch all objects at their latest version via GraphQL.
    async fn fetch_all_latest(
        &self,
        store: &GraphqlStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let object_ids = collect_all_object_ids(transaction)?;
        if object_ids.is_empty() {
            return Ok(());
        }

        let aliases: Vec<String> = object_ids
            .iter()
            .enumerate()
            .map(|(i, id)| format!(r#"obj{i}: object(address: "{id}") {{ bcs }}"#))
            .collect();

        let query = format!("{{ {} }}", aliases.join("\n"));
        self.execute_and_insert(store, &query).await
    }

    /// Fetch owned/immutable objects at their exact transaction version; fetch
    /// shared objects and packages at the latest version.
    async fn fetch_versioned(
        &self,
        store: &GraphqlStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let (versioned, latest) = split_transaction_refs(transaction)?;

        // Build a single batched query with aliases for all objects.
        let mut aliases = Vec::new();

        for (i, (id, version)) in versioned.iter().enumerate() {
            aliases.push(format!(
                r#"v{i}: object(address: "{id}", version: {}) {{ bcs }}"#,
                version.value()
            ));
        }
        for (i, id) in latest.iter().enumerate() {
            aliases.push(format!(r#"l{i}: object(address: "{id}") {{ bcs }}"#));
        }

        if !aliases.is_empty() {
            let query = format!("{{ {} }}", aliases.join("\n"));
            self.execute_and_insert(store, &query).await?;
        }

        Ok(())
    }

    /// Execute a GraphQL query and insert the resulting objects into the store.
    async fn execute_and_insert(&self, store: &GraphqlStore, query: &str) -> Result<()> {
        let json = self
            .client
            .execute(query.to_string(), vec![])
            .await
            .map_err(|e| anyhow::anyhow!("GraphQL query failed: {e}"))?;

        let data = json
            .get("data")
            .ok_or_else(|| anyhow::anyhow!("missing data in GraphQL response"))?;

        // Iterate over all fields in the response data (aliased object results).
        if let Some(obj_map) = data.as_object() {
            for (alias, value) in obj_map {
                if let Some(bcs_b64) = value.pointer("/bcs").and_then(|v| v.as_str()) {
                    let bytes = BASE64
                        .decode(bcs_b64)
                        .map_err(|e| anyhow::anyhow!("failed to decode Base64 for {alias}: {e}"))?;
                    let obj: Object = bcs::from_bytes(&bytes).map_err(|e| {
                        anyhow::anyhow!("failed to deserialize BCS for {alias}: {e}")
                    })?;
                    store.insert(obj);
                } else if !value.is_null() {
                    tracing::warn!("GraphQL alias {alias}: missing bcs field");
                }
            }
        }

        Ok(())
    }
}

/// Parse an ISO 8601 / RFC 3339 timestamp string to milliseconds since epoch.
pub(crate) fn chrono_to_millis(s: &str) -> Option<u64> {
    // GraphQL DateTime is typically a Unix timestamp in ms as a string, or an
    // ISO 8601 date. Try parsing as a plain integer first (ms since epoch).
    if let Ok(ms) = s.parse::<u64>() {
        return Some(ms);
    }
    // Fallback: try parsing as a float (seconds.millis).
    if let Ok(secs) = s.parse::<f64>() {
        return Some((secs * 1000.0) as u64);
    }
    None
}
