// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Offline execution of a staking transaction with pre-fetched
//! objects.
//!
//! This example demonstrates a two-phase workflow:
//! 1. **Fetch phase**: Connect to a node via gRPC and fetch all objects
//!    referenced by the transaction (plus protocol config and epoch info).
//! 2. **Execute phase**: Disconnect from the network and execute the
//!    transaction fully offline using `OfflineExecutor`.
//!
//! This is useful when you want to fetch objects once and then execute multiple
//! transactions offline, or when you need to execute in an air-gapped
//! environment after transferring the objects.
//!
//! The transaction calls `0x3::iota_system::request_add_stake` which requires:
//! - The shared IotaSystemState object (0x5)
//! - An owned coin object to stake
//! - A validator address to delegate to
//!
//! Usage:
//!   cargo run --example offline_stake_inspect

use anyhow::Result;
use iota_grpc_client::Client as GrpcClient;
use iota_local_executor::{ChainInfo, InMemoryStore, OfflineExecutor, VmChecks};
use iota_sdk_types::ObjectId;
use iota_types::{
    base_types::ObjectID,
    effects::TransactionEffectsAPI,
    object::Object,
    transaction::{TransactionData, TransactionDataAPI},
};

const DEVNET_GRPC: &str = "https://api.devnet.iota.cafe";

#[tokio::main]
async fn main() -> Result<()> {
    // ---------------------------------------------------------------
    // Phase 1: Fetch everything from the network
    // ---------------------------------------------------------------
    println!("Phase 1: Fetching objects from devnet via gRPC...");
    let client = GrpcClient::connect(DEVNET_GRPC).await?;

    // Bundle protocol version + RGP + epoch id + epoch timestamp into a
    // single helper call (one round-trip per field on devnet, but the helper
    // is the canonical way and parallels `GrpcExecutor::with_chain_info`).
    let info = ChainInfo::fetch_from_grpc(&client).await?;
    println!("  Protocol version: {:?}", info.protocol_version);
    println!("  Reference gas price: {}", info.reference_gas_price);
    println!("  Epoch: {}", info.epoch_id);

    // Same staking transaction bytes as the gRPC-backed staking flows.
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

    println!("  Objects to fetch: {}", object_ids.len());
    for id in &object_ids {
        println!("    - {id}");
    }

    // Fetch all objects in a single batch gRPC call (latest versions).
    let refs: Vec<(ObjectId, Option<iota_sdk_types::Version>)> =
        object_ids.iter().map(|id| (*id, None)).collect();
    let proto_objects = client.get_objects(&refs, None).await?.into_inner();

    let mut store = InMemoryStore::new();
    for proto_obj in proto_objects {
        let sdk_obj = proto_obj.object()?;
        let obj: Object = sdk_obj.try_into()?;
        println!("    Fetched {} (v{})", obj.id(), obj.version().as_u64());
        store.insert(obj);
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
