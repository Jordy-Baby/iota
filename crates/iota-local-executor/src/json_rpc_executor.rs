// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`JsonRpcExecutor`] — fetches objects from a remote node via JSON-RPC and
//! executes locally.
//!
//! All methods shared with [`crate::GrpcExecutor`] and
//! [`crate::GraphqlExecutor`] (the eight `simulate*` methods, fetch-mode
//! getters/setters, store accessors, event decoding) are emitted by the
//! [`networked_executor_methods!`] macro — see
//! [`crate::networked_helpers`] for the full surface.

use anyhow::{Result, anyhow, bail};
use iota_json_rpc_types::{
    IotaGetPastObjectRequest, IotaObjectDataOptions, IotaPastObjectResponse,
};
use iota_sdk::IotaClient;
use iota_types::{object::Object, transaction::TransactionData};

use crate::{
    ChainInfo, DebugConfig,
    caching_store::{JsonRpcFetcher, JsonRpcStore, ObjectFetchMode},
    execution::{ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    networked_helpers::networked_executor_methods,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node via JSON-RPC.
///
/// The executor owns a persistent [`JsonRpcStore`] that caches fetched
/// objects (packages, immutable objects, and mutable objects from the last
/// fetch) across [`Self::simulate_transaction`] calls. Framework packages
/// and other immutable objects are therefore fetched at most once per
/// executor. Call [`Self::clear_cache`] to drop the cache when staleness
/// matters, or use [`Self::simulate_transaction_with_debug_using`] with a
/// caller-supplied store for isolated one-off runs.
pub struct JsonRpcExecutor {
    client: IotaClient,
    env: ExecutionEnv,
    fetch_mode: ObjectFetchMode,
    shared_store: JsonRpcStore,
}

impl JsonRpcExecutor {
    /// Create a new `JsonRpcExecutor` connected to the given IOTA JSON-RPC
    /// endpoint. Uses [`ObjectFetchMode::UseTransactionVersions`] by default.
    pub async fn new(client: IotaClient) -> Result<Self> {
        Self::with_debug(client, DebugConfig::default()).await
    }

    /// Create a new `JsonRpcExecutor` with a custom [`DebugConfig`] for
    /// debug-print capture, gas profiling, and/or execution tracing.
    pub async fn with_debug(client: IotaClient, debug_config: DebugConfig) -> Result<Self> {
        let info = ChainInfo::fetch_from_json_rpc(&client).await?;
        Self::with_chain_info_and_debug(client, info, debug_config)
    }

    networked_executor_methods!(IotaClient, JsonRpcFetcher, JsonRpcStore);

    /// Fetch all objects referenced by the transaction from the remote node.
    async fn prefetch_objects(
        &self,
        store: &JsonRpcStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let options = IotaObjectDataOptions::full_content()
            .with_bcs()
            .with_owner()
            .with_previous_transaction();

        match self.fetch_mode {
            ObjectFetchMode::UseLatestVersions => {
                self.fetch_all_latest(store, transaction, options).await
            }
            ObjectFetchMode::UseTransactionVersions => {
                self.fetch_versioned(store, transaction, options).await
            }
        }
    }

    /// Fetch all objects at their latest version.
    async fn fetch_all_latest(
        &self,
        store: &JsonRpcStore,
        transaction: &TransactionData,
        options: IotaObjectDataOptions,
    ) -> Result<()> {
        let object_ids = collect_all_object_ids(transaction)?;
        if object_ids.is_empty() {
            return Ok(());
        }

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

        Ok(())
    }

    /// Fetch owned/immutable objects at their exact transaction version; fetch
    /// shared objects and packages at the latest version.
    async fn fetch_versioned(
        &self,
        store: &JsonRpcStore,
        transaction: &TransactionData,
        options: IotaObjectDataOptions,
    ) -> Result<()> {
        let (versioned, latest) = split_transaction_refs(transaction)?;

        // Fetch versioned objects at their exact versions.
        if !versioned.is_empty() {
            let requests: Vec<IotaGetPastObjectRequest> = versioned
                .into_iter()
                .map(|(object_id, version)| IotaGetPastObjectRequest { object_id, version })
                .collect();

            let responses = self
                .client
                .read_api()
                .try_multi_get_parsed_past_object(requests, options.clone())
                .await?;

            for response in responses {
                store.insert(past_object_to_object(response)?);
            }
        }

        // Fetch shared objects and packages at the latest version.
        if !latest.is_empty() {
            let responses = self
                .client
                .read_api()
                .multi_get_object_with_options(latest, options)
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

/// Convert an `IotaPastObjectResponse` into an `Object`, returning a
/// descriptive error for non-success variants.
fn past_object_to_object(response: IotaPastObjectResponse) -> Result<Object> {
    match response {
        IotaPastObjectResponse::VersionFound(data) => Ok(data.try_into()?),
        IotaPastObjectResponse::ObjectNotExists(id) => {
            bail!("object {id} does not exist")
        }
        IotaPastObjectResponse::ObjectDeleted(objref) => {
            bail!(
                "object {} at version {} is deleted",
                objref.object_id,
                objref.version
            )
        }
        IotaPastObjectResponse::VersionNotFound(id, version) => {
            bail!("object {id} not found at version {version} (may have been pruned)")
        }
        IotaPastObjectResponse::VersionTooHigh {
            object_id,
            asked_version,
            latest_version,
        } => Err(anyhow!(
            "object {object_id} version {asked_version} is higher than latest {latest_version}"
        )),
    }
}
