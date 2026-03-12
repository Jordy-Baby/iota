// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Local dev-inspect of a transaction without sending it to a node.
//!
//! This example takes base64-encoded transaction bytes (the same format used
//! by the JSON-RPC API) and executes them locally using the Move VM.
//!
//! The example transaction calls `0x2::hash::blake2b256` with `[0, 1, 2]` as
//! input — a pure function call that doesn't require any on-chain objects.
//!
//! Usage:
//!   cargo run --example local_dev_inspect

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

    // Base64-encoded transaction bytes for: 0x2::hash::blake2b256([0, 1, 2])
    let tx_bytes_base64 = "AAABAAQDAAECAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgRoYXNoCmJsYWtlMmIyNTYAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6AMAAAAAAAAAypo7AAAAAAA=";

    // Decode the transaction
    let tx_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_base64)?;
    let transaction: TransactionData = bcs::from_bytes(&tx_bytes)?;

    println!("Executing transaction locally (dev-inspect mode)...");
    println!("  Sender: {:?}", transaction.sender());
    println!("  Kind: {:?}", transaction.kind());

    // Execute locally — this is the local alternative to client.dry_run_tx()
    let result = executor
        .simulate_transaction(transaction, VmChecks::Disabled)
        .await?;

    // Check for execution errors
    if let Err(ref err) = result.execution_result {
        eprintln!("Execution failed: {err}");
        return Ok(());
    }

    println!("\nExecution succeeded!");
    println!("  Effects status: {:?}", result.effects.status());
    if let Some(ref events) = result.events {
        println!("  Events: {}", events.data.len());
    }
    if let Ok(ref results) = result.execution_result {
        println!("  Command results: {}", results.len());
        for (i, (mutable_ref_outputs, return_values)) in results.iter().enumerate() {
            println!(
                "    [{i}] mutable_reference_outputs: {}",
                mutable_ref_outputs.len()
            );
            println!("    [{i}] return_values: {}", return_values.len());
        }
    }

    Ok(())
}
