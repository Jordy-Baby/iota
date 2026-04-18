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
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    ChainInfo, DebugConfig, DebugSimulateResult,
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
    /// endpoint. Uses [`ObjectFetchMode::UseTransactionVersions`] by
    /// default.
    pub async fn new(client: IotaClient) -> Result<Self> {
        Self::with_debug(client, DebugConfig::default()).await
    }

    /// Create a new `JsonRpcExecutor` with a custom [`DebugConfig`] for
    /// debug-print capture, gas profiling, and/or execution tracing.
    pub async fn with_debug(client: IotaClient, debug_config: DebugConfig) -> Result<Self> {
        let info = ChainInfo::fetch_from_json_rpc(&client).await?;
        Self::with_chain_info_and_debug(client, info, debug_config)
    }

    /// Create a new `JsonRpcExecutor` using a pre-fetched [`ChainInfo`].
    /// Skips the round-trip to the node for protocol/epoch/gas-price —
    /// useful when building many executors against the same chain state.
    pub fn with_chain_info(client: IotaClient, info: ChainInfo) -> Result<Self> {
        Self::with_chain_info_and_debug(client, info, DebugConfig::default())
    }

    /// [`Self::with_chain_info`] + a caller-supplied [`DebugConfig`].
    pub fn with_chain_info_and_debug(
        client: IotaClient,
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

        let shared_store = JsonRpcStore::new(JsonRpcFetcher(client.clone()));
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
    /// [`JsonRpcStore`] cache.
    ///
    /// - `VmChecks::Enabled` → dry-run mode (full checks, like a real
    ///   transaction).
    /// - `VmChecks::Disabled` → dev-inspect mode (relaxed Move VM checks).
    ///
    /// **Caching:** fetched packages and objects persist across calls. Use
    /// [`Self::clear_cache`] to drop them if staleness matters, or
    /// [`Self::simulate_transaction_with_debug_using`] with a fresh
    /// [`Self::new_store`] store for isolated runs.
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
        self.prefetch_objects(&self.shared_store, &transaction)
            .await?;
        execution::simulate(&self.env, &self.shared_store, transaction, checks)
    }

    /// Return a reference to the executor's persistent store. Useful for
    /// installing a [`LocalPackage`](crate::LocalPackage) override via
    /// `install_into_caching` before the first simulation.
    pub fn store(&self) -> &JsonRpcStore {
        &self.shared_store
    }

    /// Build a fresh, isolated [`JsonRpcStore`] that shares this executor's
    /// JSON-RPC client. Pair with
    /// [`Self::simulate_transaction_with_debug_using`] for runs that must
    /// not observe or pollute the persistent cache.
    pub fn new_store(&self) -> JsonRpcStore {
        JsonRpcStore::new(JsonRpcFetcher(self.client.clone()))
    }

    /// Drop all cached objects and package overrides. The next simulation
    /// will fetch from the remote again.
    pub fn clear_cache(&mut self) {
        self.shared_store = JsonRpcStore::new(JsonRpcFetcher(self.client.clone()));
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
    /// alongside the result. With a default [`DebugConfig`] the artifacts are
    /// empty and this is equivalent to [`Self::simulate_transaction`].
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
    /// caller-supplied store — lets the caller pre-install a
    /// [`LocalPackage`](crate::LocalPackage) via `install_into_caching` before
    /// simulating.
    pub async fn simulate_transaction_with_debug_using(
        &self,
        store: JsonRpcStore,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&store, &transaction).await?;
        execution::simulate_with_debug(&self.env, &store, transaction, checks)
    }

    /// Simulate a **signed** transaction locally, verifying signatures first.
    ///
    /// This works exactly like
    /// [`simulate_transaction`](Self::simulate_transaction)
    /// but additionally validates the cryptographic signatures in `signed_data`
    /// before execution. If pre-execution verification fails the error is
    /// returned without executing the transaction.
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
        store: JsonRpcStore,
        signed_data: SenderSignedData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        self.prefetch_objects(&store, signed_data.transaction_data())
            .await?;
        execution::simulate_signed_with_debug(&self.env, &store, signed_data, checks)
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
