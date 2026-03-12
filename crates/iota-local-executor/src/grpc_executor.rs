// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`GrpcExecutor`] — fetches objects from a remote node via gRPC and executes
//! locally.

use anyhow::Result;
use iota_grpc_client::Client as GrpcClient;
use iota_protocol_config::ProtocolVersion;
use iota_sdk_types::ObjectId;
use iota_types::{
    object::Object,
    transaction::TransactionData,
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    caching_store::{GrpcFetcher, GrpcStore},
    execution::{self, ExecutionEnv, collect_all_object_ids, split_transaction_refs},
    json_rpc_executor::ObjectFetchMode,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node via gRPC.
pub struct GrpcExecutor {
    client: GrpcClient,
    env: ExecutionEnv,
    fetch_mode: ObjectFetchMode,
}

impl GrpcExecutor {
    /// Create a new `GrpcExecutor` connected to the given gRPC endpoint.
    pub async fn new(client: GrpcClient) -> Result<Self> {
        let epoch = client
            .get_epoch(
                None,
                Some("epoch,reference_gas_price,start,protocol_config.protocol_version"),
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch epoch info: {e}"))?;

        let epoch_id = epoch
            .epoch_id()
            .map_err(|e| anyhow::anyhow!("missing epoch id: {e}"))?;
        let reference_gas_price = epoch
            .gas_price()
            .map_err(|e| anyhow::anyhow!("missing gas price: {e}"))?;
        let epoch_timestamp_ms = epoch
            .start_ms()
            .map_err(|e| anyhow::anyhow!("missing epoch start: {e}"))?;
        let protocol_version = epoch
            .protocol_config()
            .and_then(|pc| pc.version())
            .map_err(|e| anyhow::anyhow!("missing protocol version: {e}"))?;

        let env = ExecutionEnv::new(
            ProtocolVersion::new(protocol_version),
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
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
    /// See [`crate::JsonRpcExecutor::simulate_transaction`] for details on
    /// `VmChecks` and shared-object limitations.
    pub async fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let store = GrpcStore::new(GrpcFetcher(self.client.clone()));
        self.prefetch_objects(&store, &transaction).await?;
        execution::simulate(&self.env, &store, transaction, checks)
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
            .map_err(|e| anyhow::anyhow!("failed to fetch objects via gRPC: {e}"))?;

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
