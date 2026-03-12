// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! [`LocalExecutor`] — fetches objects from a remote node and executes locally.

use anyhow::Result;
use iota_json_rpc_types::IotaObjectDataOptions;
use iota_sdk::IotaClient;
use iota_types::{
    base_types::ObjectID,
    object::Object,
    transaction::{TransactionData, TransactionDataAPI},
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    execution::{self, ExecutionEnv},
    RemoteStore,
};

/// A local executor that runs Move VM transactions by fetching objects from a
/// remote node.
pub struct LocalExecutor {
    client: IotaClient,
    env: ExecutionEnv,
}

impl LocalExecutor {
    /// Create a new `LocalExecutor` connected to the given IOTA JSON-RPC
    /// endpoint.
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

        Ok(Self { client, env })
    }

    /// Simulate a transaction locally.
    ///
    /// - `VmChecks::Enabled` → dry-run mode (full checks, like a real
    ///   transaction).
    /// - `VmChecks::Disabled` → dev-inspect mode (relaxed Move VM checks).
    pub async fn simulate_transaction(
        &self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let store = RemoteStore::new(self.client.clone());
        self.prefetch_objects(&store, &transaction).await?;
        execution::simulate(&self.env, &store, transaction, checks)
    }

    /// Fetch all objects referenced by the transaction from the remote node.
    async fn prefetch_objects(
        &self,
        store: &RemoteStore,
        transaction: &TransactionData,
    ) -> Result<()> {
        let input_object_kinds = transaction.input_objects()?;
        let receiving_object_refs = transaction.receiving_objects();

        let options = IotaObjectDataOptions::full_content()
            .with_bcs()
            .with_owner()
            .with_previous_transaction();

        // Collect all unique object IDs to fetch.
        let mut object_ids: Vec<ObjectID> = input_object_kinds
            .iter()
            .map(|kind| kind.object_id())
            .collect();
        for gas_ref in transaction.gas() {
            object_ids.push(gas_ref.0);
        }
        for objref in &receiving_object_refs {
            object_ids.push(objref.0);
        }
        object_ids.sort();
        object_ids.dedup();

        if !object_ids.is_empty() {
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
        }

        Ok(())
    }
}
