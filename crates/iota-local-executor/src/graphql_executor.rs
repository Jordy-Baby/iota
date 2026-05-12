// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`GraphqlExecutor`] — fetches objects from a remote node via GraphQL and
//! executes locally.
//!
//! All methods shared with [`crate::GrpcExecutor`] are emitted by the
//! [`networked_executor_methods!`] macro — see [`crate::networked_helpers`]
//! for the full surface.

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iota_graphql_rpc_client::simple_client::SimpleClient;
use iota_types::{object::Object, transaction::TransactionData};

use crate::{
    ChainInfo, DebugConfig,
    caching_store::{GraphqlFetcher, GraphqlStore, ObjectFetchMode},
    execution::{ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    networked_helpers::networked_executor_methods,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node via GraphQL.
///
/// The executor owns a persistent [`GraphqlStore`] that caches fetched
/// objects across simulation calls. See [`crate::GrpcExecutor`] for the
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

    networked_executor_methods!(SimpleClient, GraphqlFetcher, GraphqlStore);

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
                version.as_u64()
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

/// Parse the GraphQL `startTimestamp` field to milliseconds since epoch.
///
/// IOTA's GraphQL schema reports `startTimestamp` as a string. Two encodings
/// are accepted:
///
/// - integer string — already milliseconds since epoch (returned as-is)
/// - floating-point string — seconds-since-epoch with sub-second precision
///   (multiplied by 1000 and truncated)
///
/// Returns `None` for any other shape (e.g. an ISO 8601 / RFC 3339 date).
pub(crate) fn parse_start_timestamp_millis(s: &str) -> Option<u64> {
    if let Ok(ms) = s.parse::<u64>() {
        return Some(ms);
    }
    if let Ok(secs) = s.parse::<f64>() {
        return Some((secs * 1000.0) as u64);
    }
    None
}
