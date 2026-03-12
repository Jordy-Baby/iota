// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`JsonRpcExecutor`] — fetches objects from a remote node via JSON-RPC and
//! executes locally.

use anyhow::{Result, anyhow, bail};
use iota_json_rpc_types::{
    IotaGetPastObjectRequest, IotaObjectDataOptions, IotaPastObjectResponse,
};
use iota_sdk::IotaClient;
use iota_types::{
    object::Object,
    transaction::TransactionData,
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    caching_store::{JsonRpcFetcher, JsonRpcStore},
    execution::{self, ExecutionEnv, collect_all_object_ids, split_transaction_refs},
};

/// Controls how objects are fetched from the remote node.
#[derive(Debug, Clone, Copy, Default)]
pub enum ObjectFetchMode {
    /// Fetch owned/immutable objects at the exact version specified in the
    /// transaction. Shared objects and packages are always fetched at the
    /// latest version. This is the default and matches what a real node
    /// would see when processing the transaction.
    #[default]
    UseTransactionVersions,

    /// Fetch all objects at their latest version, ignoring the versions in the
    /// transaction. Useful for dev-inspect style usage where the transaction
    /// may have been constructed with stale or placeholder object references.
    UseLatestVersions,
}

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node.
pub struct JsonRpcExecutor {
    client: IotaClient,
    env: ExecutionEnv,
    fetch_mode: ObjectFetchMode,
}

impl JsonRpcExecutor {
    /// Create a new `JsonRpcExecutor` connected to the given IOTA JSON-RPC
    /// endpoint. Uses [`ObjectFetchMode::UseTransactionVersions`] by
    /// default.
    pub async fn new(client: IotaClient) -> Result<Self> {
        let reference_gas_price = client.read_api().get_reference_gas_price().await?;

        let protocol_config_response = client.read_api().get_protocol_config(None).await?;
        let protocol_version = protocol_config_response.protocol_version;

        let latest_checkpoint = client
            .read_api()
            .get_latest_checkpoint_sequence_number()
            .await?;
        let checkpoint = client
            .read_api()
            .get_checkpoint(latest_checkpoint.into())
            .await?;

        let env = ExecutionEnv::new(
            protocol_version,
            reference_gas_price,
            checkpoint.epoch,
            checkpoint.timestamp_ms,
        )?;

        Ok(Self {
            client,
            env,
            fetch_mode: ObjectFetchMode::default(),
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

    /// Simulate a transaction locally.
    ///
    /// - `VmChecks::Enabled` → dry-run mode (full checks, like a real
    ///   transaction).
    /// - `VmChecks::Disabled` → dev-inspect mode (relaxed Move VM checks).
    ///
    /// **Note on shared objects:** Shared objects are always fetched at the
    /// latest version because their version is assigned by consensus and is
    /// not encoded in the transaction. This means the local simulation may
    /// see a different shared-object state than the network would ultimately
    /// use when the transaction is executed for real.
    pub async fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let store = JsonRpcStore::new(JsonRpcFetcher(self.client.clone()));
        self.prefetch_objects(&store, &transaction).await?;
        execution::simulate(&self.env, &store, transaction, checks)
    }

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
