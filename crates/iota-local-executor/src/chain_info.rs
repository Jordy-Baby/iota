// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Cached chain-environment information for reuse across executor instances.
//!
//! Each networked executor (`GrpcExecutor`, `GraphqlExecutor`) fetches four
//! pieces of chain state when it is constructed: the protocol version,
//! reference gas price, current epoch id, and epoch-start timestamp. Fetching
//! these four values per executor is wasteful when callers create many
//! executors in a loop (e.g. one per transaction in a batch). [`ChainInfo`]
//! captures the four values in a small `Copy` bundle that can be fetched once
//! and reused:
//!
//! ```ignore
//! let info = ChainInfo::fetch_from_grpc(&client).await?;
//! for tx in txs {
//!     let executor = GrpcExecutor::with_chain_info(client.clone(), info)?;
//!     let r = executor.simulate_transaction(tx, VmChecks::Disabled).await?;
//!     …
//! }
//! ```
//!
//! The values don't meaningfully drift across short time windows — epoch
//! transitions happen on the order of minutes to hours — so the same
//! [`ChainInfo`] can be reused for the duration of a batch.

use anyhow::Result;
use iota_protocol_config::ProtocolVersion;

/// Snapshot of the chain state each local executor needs to build its
/// `ExecutionEnv`. See the module docs for how to reuse this across many
/// executor instances.
#[derive(Debug, Clone, Copy)]
pub struct ChainInfo {
    pub protocol_version: ProtocolVersion,
    pub reference_gas_price: u64,
    pub epoch_id: u64,
    pub epoch_timestamp_ms: u64,
}

impl ChainInfo {
    /// Fetch the four values from a gRPC endpoint.
    pub async fn fetch_from_grpc(client: &iota_grpc_client::Client) -> Result<Self> {
        let epoch = client
            .get_epoch(
                None,
                Some("epoch,reference_gas_price,start,protocol_config.protocol_version"),
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch epoch info: {e}"))?
            .into_inner();
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
        Ok(Self {
            protocol_version: ProtocolVersion::new(protocol_version),
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
        })
    }

    /// Fetch the four values from a GraphQL endpoint.
    pub async fn fetch_from_graphql(
        client: &iota_graphql_rpc_client::simple_client::SimpleClient,
    ) -> Result<Self> {
        let query = r#"{
            epoch {
                epochId
                referenceGasPrice
                startTimestamp
                protocolConfigs { protocolVersion }
            }
        }"#;
        let json = client
            .execute(query.to_string(), vec![])
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch epoch info via GraphQL: {e}"))?;
        let epoch = json
            .pointer("/data/epoch")
            .ok_or_else(|| anyhow::anyhow!("missing epoch data in GraphQL response"))?;
        let epoch_id = epoch
            .get("epochId")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("missing epochId"))?;
        let reference_gas_price = epoch
            .get("referenceGasPrice")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| anyhow::anyhow!("missing referenceGasPrice"))?;
        let epoch_timestamp_ms = epoch
            .get("startTimestamp")
            .and_then(|v| v.as_str())
            .and_then(crate::graphql_executor::parse_start_timestamp_millis)
            .ok_or_else(|| anyhow::anyhow!("missing startTimestamp"))?;
        let protocol_version = epoch
            .pointer("/protocolConfigs/protocolVersion")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("missing protocolVersion"))?;
        Ok(Self {
            protocol_version: ProtocolVersion::new(protocol_version),
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
        })
    }
}
