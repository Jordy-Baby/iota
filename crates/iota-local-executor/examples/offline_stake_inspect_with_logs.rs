// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Offline execution of a staking transaction with pre-fetched
//! objects.
//!
//! This example demonstrates a two-phase workflow:
//! 1. **Fetch phase**: Connect to a node and fetch all objects referenced by
//!    the transaction, including dynamic field children needed during
//!    execution.
//! 2. **Execute phase**: Disconnect from the network and execute the
//!    transaction fully offline using `OfflineExecutor`.
//!
//! Complex transactions like staking access dynamic field children (e.g., the
//! validator table inside IotaSystemState). These children must also be fetched
//! and provided to the offline store. This example recursively fetches all
//! dynamic fields to ensure everything is available.
//!
//! The transaction calls `0x3::iota_system::request_add_stake` which requires:
//! - The shared IotaSystemState object (0x5)
//! - An owned coin object to stake
//! - A validator address to delegate to
//!
//! Usage:
//!   cargo run --example offline_stake_inspect_with_logs

use std::collections::BTreeSet;

use anyhow::Result;
use iota_framework::BuiltInFramework;
use iota_json_rpc_types::IotaObjectDataOptions;
use iota_local_executor::{InMemoryStore, OfflineExecutor, VmChecks};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_sdk::{IotaClient, IotaClientBuilder};
use iota_types::{
    base_types::ObjectID,
    effects::TransactionEffectsAPI,
    object::Object,
    transaction::{TransactionData, TransactionDataAPI},
};

/// Recursively fetch all dynamic field children of the given object IDs.
/// Returns all fetched child objects.
async fn fetch_dynamic_field_children(
    client: &IotaClient,
    parent_ids: &[ObjectID],
    options: &IotaObjectDataOptions,
) -> Result<Vec<Object>> {
    let mut all_children = Vec::new();
    let mut visited: BTreeSet<ObjectID> = BTreeSet::new();
    let mut queue: Vec<ObjectID> = parent_ids.to_vec();

    while let Some(parent_id) = queue.pop() {
        if !visited.insert(parent_id) {
            continue;
        }

        // Paginate through all dynamic fields of this parent
        let mut cursor = None;
        loop {
            let page = client
                .read_api()
                .get_dynamic_fields(parent_id, cursor, Some(50))
                .await?;

            if page.data.is_empty() {
                break;
            }

            // Collect the child object IDs
            let child_ids: Vec<ObjectID> = page.data.iter().map(|info| info.object_id).collect();

            // Fetch the actual child objects
            let responses = client
                .read_api()
                .multi_get_object_with_options(child_ids, options.clone())
                .await?;

            for response in responses {
                if let Some(data) = response.data {
                    let obj: Object = data.try_into()?;
                    // Also recurse into this child's dynamic fields
                    queue.push(obj.id());
                    all_children.push(obj);
                }
            }

            if !page.has_next_page {
                break;
            }
            cursor = page.next_cursor;
        }
    }

    Ok(all_children)
}

#[tokio::main]
async fn main() -> Result<()> {
    // ---------------------------------------------------------------
    // Phase 1: Fetch everything from the network
    // ---------------------------------------------------------------
    println!("Phase 1: Fetching objects from devnet...");
    let client = IotaClientBuilder::default().build_devnet().await?;

    // Fetch protocol config and epoch info
    let protocol_config_response = client.read_api().get_protocol_config(None).await?;
    let protocol_version: ProtocolVersion = protocol_config_response.protocol_version;
    let reference_gas_price = client.read_api().get_reference_gas_price().await?;

    let latest_checkpoint = client
        .read_api()
        .get_latest_checkpoint_sequence_number()
        .await?;
    let checkpoint = client
        .read_api()
        .get_checkpoint(latest_checkpoint.into())
        .await?;
    let epoch_id = checkpoint.epoch;
    let epoch_timestamp_ms = checkpoint.timestamp_ms;

    println!("  Protocol version: {protocol_version:?}");
    println!("  Reference gas price: {reference_gas_price}");
    println!("  Epoch: {epoch_id}");

    // Same staking transaction bytes as local_stake_inspect
    let tx_bytes_base64 = "AAADAAgAypo7AAAAAAEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUBAAAAAAAAAAEAINqRtZV/6ONntsXV/L9IRp9ACpOV+VnDUxBwOyp4hRr+AgIAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbRFyZXF1ZXN0X2FkZF9zdGFrZQADAQEAAgAAAQIAIiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSIDGcvPD+/Tvw/dvzpb0UY9UJCRBlY/BhIGEGEBTRGPdHNaPxUAAAAAACA8sy5o+BES2kxKEnXM7cH94maytD+8aHx/lXKR0+CK2JbazvWIYMZYjungyM2qGZgUw31KLRDe8bX9f58VgNUkyBMAAAAAAAAgh/y1xeOPt7crDLnGZlW2jTzXSaAXYKtvOEISYwCZb63PFCwjsZFjUhgajd5ZSwA/VzogxQh/JEQL05VfqazefccTAAAAAAAAIJgclRy0Uq4ONHCvw1vi8JqDMorQT11j9eRPTBPeDMA/IiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSLoAwAAAAAAAGATQQAAAAAAAA==";

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

    // Fetch all directly-referenced objects in a single batch RPC call
    let options = IotaObjectDataOptions::full_content()
        .with_bcs()
        .with_owner()
        .with_previous_transaction();

    let responses = client
        .read_api()
        .multi_get_object_with_options(object_ids.clone(), options.clone())
        .await?;

    let mut store = InMemoryStore::new();

    // Load built-in framework packages (0x1, 0x2, 0x3, 0x107a) — these are
    // needed as transitive dependencies during execution.
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    println!("  Loaded built-in framework packages into store");

    let mut fetched_count = 0;
    for response in responses {
        if let Some(data) = response.data {
            let obj: Object = data.try_into()?;
            println!("    Fetched {} (v{})", obj.id(), obj.version().as_u64(),);
            store.insert(obj);
            fetched_count += 1;
        }
    }

    // Recursively fetch dynamic field children of all fetched objects.
    // Complex transactions (like staking) access child objects of shared
    // objects (e.g., validator table entries inside IotaSystemState).
    println!("  Fetching dynamic field children recursively...");
    let children = fetch_dynamic_field_children(&client, &object_ids, &options).await?;
    println!("    Found {} child objects", children.len());
    for obj in &children {
        println!("    Child {} (v{})", obj.id(), obj.version().as_u64(),);
    }
    for obj in children {
        store.insert(obj);
        fetched_count += 1;
    }
    println!("  Total objects fetched from network: {fetched_count}");

    // Log all non-framework objects as BCS base64 (for hardcoded_offline_inspect)
    println!("\n  === BCS-encoded objects (for offline reproduction) ===");
    println!("  protocol_version: {protocol_version:?}");
    println!("  reference_gas_price: {reference_gas_price}");
    println!("  epoch_id: {epoch_id}");
    println!("  epoch_timestamp_ms: {epoch_timestamp_ms}");
    for (id, obj) in store.iter() {
        // Skip framework packages (available via BuiltInFramework)
        if obj.is_package() {
            continue;
        }
        let bcs_bytes = bcs::to_bytes(obj)?;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bcs_bytes);
        println!("  object {id} ({} bytes): {b64}", bcs_bytes.len());
    }
    println!("  === End BCS-encoded objects ===\n");

    // ---------------------------------------------------------------
    // Phase 2: Execute fully offline (no more network access)
    // ---------------------------------------------------------------
    println!("Phase 2: Executing offline (no network access)...");

    let _protocol_config = ProtocolConfig::get_for_version(protocol_version, Chain::Unknown);

    let executor = OfflineExecutor::new(
        protocol_version,
        reference_gas_price,
        epoch_id,
        epoch_timestamp_ms,
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
