// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Local dev-inspect of a staking transaction with objects.
//!
//! This example demonstrates local execution of a transaction that involves
//! both owned objects (coins) and shared objects (IotaSystemState).
//!
//! The transaction calls `0x3::iota_system::request_add_stake` which requires:
//! - The shared IotaSystemState object (0x5)
//! - An owned coin object to stake
//! - A validator address to stake with
//!
//! Usage:
//!   cargo run --example local_stake_inspect

use anyhow::Result;
use iota_local_executor::{JsonRpcExecutor, VmChecks};
use iota_sdk::IotaClientBuilder;
use iota_types::{
    effects::TransactionEffectsAPI,
    transaction::{TransactionData, TransactionDataAPI},
};

#[tokio::main]
async fn main() -> Result<()> {
    // Connect to devnet
    let client = IotaClientBuilder::default().build_devnet().await?;

    // Create the local executor (fetches protocol config and epoch info)
    let executor = JsonRpcExecutor::new(client).await?;

    // Base64-encoded transaction bytes for: 0x3::iota_system::request_add_stake
    // This involves:
    // - Shared object: IotaSystemState (0x5)
    // - Owned objects: coins for staking and gas payment
    let tx_bytes_base64 = "AAADAAgAypo7AAAAAAEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUBAAAAAAAAAAEAINqRtZV/6ONntsXV/L9IRp9ACpOV+VnDUxBwOyp4hRr+AgIAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbRFyZXF1ZXN0X2FkZF9zdGFrZQADAQEAAgAAAQIAIiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSIDGcvPD+/Tvw/dvzpb0UY9UJCRBlY/BhIGEGEBTRGPdHNaPxUAAAAAACA8sy5o+BES2kxKEnXM7cH94maytD+8aHx/lXKR0+CK2JbazvWIYMZYjungyM2qGZgUw31KLRDe8bX9f58VgNUkyBMAAAAAAAAgh/y1xeOPt7crDLnGZlW2jTzXSaAXYKtvOEISYwCZb63PFCwjsZFjUhgajd5ZSwA/VzogxQh/JEQL05VfqazefccTAAAAAAAAIJgclRy0Uq4ONHCvw1vi8JqDMorQT11j9eRPTBPeDMA/IiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSLoAwAAAAAAAGATQQAAAAAAAA==";

    // Decode the transaction
    let tx_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_base64)?;
    let transaction: TransactionData = bcs::from_bytes(&tx_bytes)?;

    println!("Executing staking transaction locally (dev-inspect mode)...");
    println!("  Sender: {}", transaction.sender());
    println!("  Gas budget: {}", transaction.gas_budget());
    println!("  Gas price: {}", transaction.gas_price());

    // List input objects the transaction references
    let input_objects = transaction.input_objects()?;
    println!("  Input objects ({}):", input_objects.len());
    for kind in &input_objects {
        println!("    - {kind:?}");
    }
    println!("  Gas coins ({}):", transaction.gas().len());
    for gas_ref in transaction.gas() {
        println!("    - {}  v{}", gas_ref.object_id, gas_ref.version.as_u64());
    }

    // Execute locally — objects are fetched from devnet, but execution is local
    let result = executor
        .simulate_transaction(transaction, VmChecks::Disabled)
        .await?;

    // Check for execution errors
    if let Err(ref err) = result.execution_result {
        eprintln!("\nExecution failed: {err}");
        println!("  Effects status: {:?}", result.effects.status());
        return Ok(());
    }

    println!("\nExecution succeeded!");
    println!("  Effects status: {:?}", result.effects.status());
    println!("  Objects modified: {}", result.effects.mutated().len());
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
