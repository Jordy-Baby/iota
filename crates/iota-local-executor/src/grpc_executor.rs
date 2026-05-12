// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`GrpcExecutor`] — fetches objects from a remote node via gRPC and executes
//! locally.
//!
//! All methods shared with [`crate::JsonRpcExecutor`] and
//! [`crate::GraphqlExecutor`] are emitted by the
//! [`networked_executor_methods!`] macro — see
//! [`crate::networked_helpers`] for the full surface.

use anyhow::Result;
use iota_grpc_client::Client as GrpcClient;
use iota_sdk_types::{ObjectId, Version};
use iota_types::{object::Object, transaction::TransactionData};

use crate::{
    ChainInfo, DebugConfig,
    caching_store::{GrpcFetcher, GrpcStore, ObjectFetchMode},
    execution::{ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    networked_helpers::networked_executor_methods,
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

    networked_executor_methods!(GrpcClient, GrpcFetcher, GrpcStore);

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

        let sdk_refs: Vec<(ObjectId, Option<Version>)> =
            refs.into_iter().map(|id| (id, None)).collect();

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
            let sdk_refs: Vec<(ObjectId, Option<Version>)> = versioned
                .into_iter()
                .map(|(id, version)| (id, Some(version)))
                .collect();
            self.fetch_and_insert(store, &sdk_refs).await?;
        }

        // Fetch shared objects and packages at the latest version.
        if !latest.is_empty() {
            let sdk_refs: Vec<(ObjectId, Option<Version>)> =
                latest.into_iter().map(|id| (id, None)).collect();
            self.fetch_and_insert(store, &sdk_refs).await?;
        }

        Ok(())
    }

    /// Fetch objects from the gRPC node and insert them into the store.
    async fn fetch_and_insert(
        &self,
        store: &GrpcStore,
        refs: &[(ObjectId, Option<Version>)],
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
