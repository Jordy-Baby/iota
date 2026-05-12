// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Offline execution of a staking transaction with pre-fetched
//! objects.
//!
//! This example demonstrates a two-phase workflow:
//! 1. **Fetch phase**: Connect to a node via gRPC and fetch all objects
//!    referenced by the transaction, including dynamic field children needed
//!    during execution.
//! 2. **Execute phase**: Disconnect from the network and execute the
//!    transaction fully offline using `OfflineExecutor`.
//!
//! Complex transactions like staking access dynamic field children (e.g., the
//! validator table inside `IotaSystemState`). These children must also be
//! fetched and provided to the offline store, so the example recursively walks
//! the dynamic-field tree of every input object.
//!
//! The transaction calls `0x3::iota_system::request_add_stake` which requires:
//! - The shared `IotaSystemState` object (0x5)
//! - An owned coin object to stake
//! - A validator address to delegate to
//!
//! Usage:
//!   cargo run --example offline_stake_inspect
//!   cargo run --example offline_stake_inspect -- --dump-bcs
//!
//! Defaults to `https://grpc.devnet.iota.cafe` (IOTA devnet gRPC endpoint).
//! Override with `IOTA_GRPC_URL=<url>` to point at testnet, mainnet, or a
//! local node. The transaction bytes below were captured against devnet;
//! against a different network the referenced objects won't exist and the
//! fetch step will fail.
//!
//! Pass `--dump-bcs` to additionally print every non-framework object as
//! BCS-base64 plus the live `ChainInfo` values, so the captured snapshot can
//! be pasted into `hardcoded_offline_inspect.rs` to refresh its hardcoded
//! state.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use iota_grpc_client::Client as GrpcClient;
use iota_local_executor::{ChainInfo, InMemoryStore, OfflineExecutor, VmChecks};
use iota_sdk_types::{ObjectId, Version};
use iota_types::{
    base_types::ObjectID,
    effects::TransactionEffectsAPI,
    object::Object,
    transaction::{TransactionData, TransactionDataAPI},
};

const DEFAULT_GRPC_URL: &str = "https://grpc.devnet.iota.cafe";

#[tokio::main]
async fn main() -> Result<()> {
    let grpc_url = std::env::var("IOTA_GRPC_URL").unwrap_or_else(|_| DEFAULT_GRPC_URL.to_string());
    let dump_bcs = std::env::args().any(|arg| arg == "--dump-bcs");

    // ---------------------------------------------------------------
    // Phase 1: Fetch everything from the network
    // ---------------------------------------------------------------
    println!("Phase 1: Fetching objects via gRPC at {grpc_url}...");
    let client = GrpcClient::connect(grpc_url).await?;

    // Bundle protocol version + RGP + epoch id + epoch timestamp into a
    // single helper call (the canonical way; same helper backs
    // `GrpcExecutor::with_chain_info`).
    let info = ChainInfo::fetch_from_grpc(&client).await?;
    println!("  Protocol version: {:?}", info.protocol_version);
    println!("  Reference gas price: {}", info.reference_gas_price);
    println!("  Epoch: {}", info.epoch_id);

    // Same staking transaction bytes as the gRPC-backed staking flows.
    let tx_bytes_base64 = "AAADAAgAlDV3AAAAAAEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUBAAAAAAAAAAEAINqRtZV/6ONntsXV/L9IRp9ACpOV+VnDUxBwOyp4hRr+AgIAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbRFyZXF1ZXN0X2FkZF9zdGFrZQADAQEAAgAAAQIAHuEtyg55iWaoL3TAEMEJ4b0GdPT0dRfbaEPyI7rV63wCd+72H5zU/gt6MdaptzF/2BEJvVyZeilwKuGwtdDaCHgBAAAAAAAAACBP1tQu7fkEDhIgawXcbLBuc8pnfopybGLDMceo+Rgku7KiTuotGDfo8i49M/V+zY7RQ7089bD45vizBsUQvnkOAQAAAAAAAAAgRrgfcWFKJI6ORE4kvllfzibNTlHi46/l5t8MfFm0X0se4S3KDnmJZqgvdMAQwQnhvQZ09PR1F9toQ/IjutXrfOgDAAAAAAAAYBNBAAAAAAAA";

    let tx_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_base64)?;
    let transaction: TransactionData = bcs::from_bytes(&tx_bytes)?;

    // Collect all object IDs referenced by the transaction
    let input_object_kinds = transaction.input_objects()?;
    let mut object_ids: Vec<ObjectID> = input_object_kinds
        .iter()
        .map(|kind| kind.object_id())
        .collect();

    // Add gas payment objects
    for gas_ref in transaction.gas() {
        if !object_ids.contains(&gas_ref.object_id) {
            object_ids.push(gas_ref.object_id);
        }
    }

    // Add receiving objects
    for objref in &transaction.receiving_objects() {
        if !object_ids.contains(&objref.object_id) {
            object_ids.push(objref.object_id);
        }
    }

    object_ids.sort();
    object_ids.dedup();

    println!("  Transaction input objects: {}", object_ids.len());
    for id in &object_ids {
        println!("    - {id}");
    }

    // Fetch all directly-referenced objects in a single batch gRPC call.
    let refs: Vec<(ObjectId, Option<Version>)> = object_ids.iter().map(|id| (*id, None)).collect();
    let proto_objects = client.get_objects(&refs, None).await?.into_inner();

    // Seed the store with the built-in framework genesis packages
    // (0x1 std, 0x2 iota, 0x3 iota_system, 0x107a stardust, …). The Move VM
    // resolves `use 0x1::option` / `0x2::iota` / etc. against this set during
    // execution, so they must be present even when not listed as transaction
    // inputs.
    let mut store = InMemoryStore::with_framework();
    println!("  Loaded built-in framework packages into store");

    let mut fetched_count = 0;
    for proto_obj in proto_objects {
        let sdk_obj = proto_obj.object()?;
        let obj: Object = sdk_obj.try_into()?;
        println!("    Fetched {} (v{})", obj.id(), obj.version().as_u64());
        store.insert(obj);
        fetched_count += 1;
    }

    // Recursively fetch dynamic field children of all fetched objects.
    // Complex transactions (like staking) access child objects of shared
    // objects (e.g., validator table entries inside `IotaSystemState`).
    println!("  Fetching dynamic field children recursively...");
    let children = fetch_dynamic_field_children(&client, &object_ids).await?;
    println!("    Found {} child objects", children.len());
    for obj in &children {
        println!("    Child {} (v{})", obj.id(), obj.version().as_u64());
    }
    for obj in children {
        store.insert(obj);
        fetched_count += 1;
    }
    println!("  Total objects fetched from network: {fetched_count}");

    // Optional: dump every non-framework object as BCS-base64 plus the live
    // `ChainInfo` snapshot. Re-run with `--dump-bcs` to capture this; copy
    // the output into `hardcoded_offline_inspect.rs` to refresh its hardcoded
    // state when the framework drifts and breaks the snapshot.
    if dump_bcs {
        println!("\n  === BCS-encoded objects (for offline reproduction) ===");
        println!("  protocol_version: {:?}", info.protocol_version);
        println!("  reference_gas_price: {}", info.reference_gas_price);
        println!("  epoch_id: {}", info.epoch_id);
        println!("  epoch_timestamp_ms: {}", info.epoch_timestamp_ms);
        for (id, obj) in store.iter() {
            // Skip framework packages (available via BuiltInFramework).
            if obj.is_package() {
                continue;
            }
            let bcs_bytes = bcs::to_bytes(obj)?;
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bcs_bytes);
            println!("  object {id} ({} bytes): {b64}", bcs_bytes.len());
        }
        println!("  === End BCS-encoded objects ===\n");
    }

    // ---------------------------------------------------------------
    // Phase 2: Execute fully offline (no more network access)
    // ---------------------------------------------------------------
    println!("\nPhase 2: Executing offline (no network access)...");

    let executor = OfflineExecutor::new(
        info.protocol_version,
        info.reference_gas_price,
        info.epoch_id,
        info.epoch_timestamp_ms,
        store,
    )?;

    println!("  Sender: {}", transaction.sender());
    println!("  Gas budget: {}", transaction.gas_budget());

    let result = executor.simulate_transaction(transaction, VmChecks::Disabled)?;

    // ---------------------------------------------------------------
    // Print results
    // ---------------------------------------------------------------
    if let Err(ref err) = result.execution_result {
        eprintln!("\nExecution failed: {err}");
        println!("  Effects status: {:?}", result.effects.status());
        return Ok(());
    }

    println!("\nExecution succeeded!");
    println!("  Effects status: {:?}", result.effects.status());
    println!("  Objects mutated: {}", result.effects.mutated().len());
    println!("  Objects created: {}", result.effects.created().len());
    if let Some(ref events) = result.events {
        println!("  Events: {}", events.data.len());
        for event in &events.data {
            println!("    - {}::{}", event.type_.module(), event.type_.name());
        }
    }
    if let Ok(ref results) = result.execution_result {
        println!("  Command results: {}", results.len());
        for (i, (mutable_ref_outputs, return_values)) in results.iter().enumerate() {
            println!(
                "    [{i}] mutable_reference_outputs: {}, return_values: {}",
                mutable_ref_outputs.len(),
                return_values.len()
            );
        }
    }

    Ok(())
}

/// Recursively fetch all dynamic field children of the given object IDs. The
/// staking call reads the validator table inside `IotaSystemState`, which is a
/// dynamic field, so the offline executor needs those children in its store
/// even though they aren't listed as transaction inputs.
async fn fetch_dynamic_field_children(
    client: &GrpcClient,
    parent_ids: &[ObjectID],
) -> Result<Vec<Object>> {
    let mut all_children = Vec::new();
    let mut visited: BTreeSet<ObjectID> = BTreeSet::new();
    let mut queue: Vec<ObjectID> = parent_ids.to_vec();

    while let Some(parent_id) = queue.pop() {
        if !visited.insert(parent_id) {
            continue;
        }

        // Auto-paginate through all dynamic fields of this parent.
        let page = client
            .list_dynamic_fields(parent_id, None, None, None)
            .collect(None)
            .await
            .map_err(|e| anyhow!("list_dynamic_fields({parent_id}) failed: {e}"))?;

        let fields = page.into_inner();
        if fields.is_empty() {
            continue;
        }

        // Collect the child object IDs. We always need `field_id` (the field
        // object itself). For dynamic object fields the actual value lives in
        // `child_id`; include both when present.
        let mut child_ids: Vec<(ObjectId, Option<Version>)> = Vec::new();
        for field in &fields {
            if let Some(fid) = field.field_id.as_ref() {
                let sdk: ObjectId =
                    fid.try_into().map_err(|e| anyhow!("invalid field_id: {e}"))?;
                child_ids.push((sdk, None));
            }
            if let Some(cid) = field.child_id.as_ref() {
                let sdk: ObjectId =
                    cid.try_into().map_err(|e| anyhow!("invalid child_id: {e}"))?;
                child_ids.push((sdk, None));
            }
        }

        if child_ids.is_empty() {
            continue;
        }

        // Fetch the child objects, and recurse into each child's own dynamic
        // fields.
        let proto_objs = client.get_objects(&child_ids, None).await?.into_inner();
        for proto_obj in proto_objs {
            let sdk_obj = proto_obj.object()?;
            let obj: Object = sdk_obj.try_into()?;
            queue.push(obj.id());
            all_children.push(obj);
        }
    }

    Ok(all_children)
}
