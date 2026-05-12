// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `--remote-grpc` / `--remote-object` handling: connect to a gRPC node, fetch
//! requested objects, and snapshot the chain's protocol version / reference
//! gas price / epoch so the executor mirrors live chain state.

use anyhow::{Context, Result, anyhow, bail};
use iota_grpc_client::Client as GrpcClient;
use iota_local_executor::ChainInfo;
use iota_protocol_config::ProtocolVersion;
use iota_sdk_types::{ObjectId, Version};
use iota_types::{base_types::ObjectID, object::Object};

/// Chain environment to feed into the offline executor.
pub(crate) struct ResolvedEnv {
    pub(crate) protocol_version: ProtocolVersion,
    pub(crate) reference_gas_price: Option<u64>,
    pub(crate) epoch_id: u64,
    pub(crate) epoch_timestamp_ms: u64,
    pub(crate) fetched_objects: Vec<Object>,
}

impl ResolvedEnv {
    fn offline() -> Self {
        Self {
            protocol_version: ProtocolVersion::MAX,
            reference_gas_price: None,
            epoch_id: 0,
            epoch_timestamp_ms: 0,
            fetched_objects: Vec::new(),
        }
    }
}

/// Resolve the chain environment.
///
/// - With `--remote-grpc`: connect, fetch the requested `--remote-object` IDs,
///   and snapshot live chain state via [`ChainInfo::fetch_from_grpc`].
/// - Without `--remote-grpc`: synthesise an offline environment with
///   `protocol_version = ProtocolVersion::MAX`, epoch 0, and the caller's
///   `--gas-price` as the reference gas price.
pub(crate) fn resolve_env(
    remote_grpc: Option<&str>,
    remote_objects: &[String],
) -> Result<ResolvedEnv> {
    let Some(url) = remote_grpc else {
        if !remote_objects.is_empty() {
            bail!("--remote-object requires --remote-grpc URL");
        }
        return Ok(ResolvedEnv::offline());
    };

    let rt = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    rt.block_on(async {
        let client = GrpcClient::connect(url.to_string())
            .await
            .with_context(|| format!("connecting to gRPC endpoint `{url}`"))?;

        let info = ChainInfo::fetch_from_grpc(&client)
            .await
            .context("fetching chain info via gRPC")?;

        let refs = parse_remote_object_specs(remote_objects)?;
        let fetched_objects = if refs.is_empty() {
            Vec::new()
        } else {
            fetch_objects(&client, &refs).await?
        };

        Ok(ResolvedEnv {
            protocol_version: info.protocol_version,
            reference_gas_price: Some(info.reference_gas_price),
            epoch_id: info.epoch_id,
            epoch_timestamp_ms: info.epoch_timestamp_ms,
            fetched_objects,
        })
    })
}

fn parse_remote_object_specs(specs: &[String]) -> Result<Vec<(ObjectId, Option<Version>)>> {
    specs.iter().map(|s| parse_remote_object_spec(s)).collect()
}

fn parse_remote_object_spec(spec: &str) -> Result<(ObjectId, Option<Version>)> {
    let (id_part, version_part) = match spec.split_once('@') {
        Some((id, v)) => (id, Some(v)),
        None => (spec, None),
    };
    let id = ObjectID::from_prefixed_short_hex(id_part.trim())
        .with_context(|| format!("parsing object id in --remote-object `{spec}`"))?;
    let version = match version_part {
        Some(v) => {
            let n: u64 = v
                .trim()
                .parse()
                .with_context(|| format!("parsing version in --remote-object `{spec}`"))?;
            Some(Version::from(n))
        }
        None => None,
    };
    Ok((ObjectId::from(id), version))
}

async fn fetch_objects(
    client: &GrpcClient,
    refs: &[(ObjectId, Option<Version>)],
) -> Result<Vec<Object>> {
    let proto_objects = client
        .get_objects(refs, None)
        .await
        .map_err(|e| anyhow!("failed to fetch --remote-object set via gRPC: {e}"))?
        .into_inner();

    let mut out = Vec::with_capacity(proto_objects.len());
    for proto_obj in proto_objects {
        let sdk_obj = proto_obj
            .object()
            .map_err(|e| anyhow!("failed to deserialize fetched gRPC object: {e}"))?;
        let obj: Object = sdk_obj
            .try_into()
            .map_err(|e| anyhow!("failed to convert fetched gRPC object: {e}"))?;
        out.push(obj);
    }
    Ok(out)
}
