// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: Fully offline execution of a Move transaction.
//!
//! This example demonstrates running the Move VM with zero network access.
//! All required objects (framework packages) are provided from the built-in
//! framework, and the transaction is executed entirely in-memory.
//!
//! The transaction calls `0x2::hash::blake2b256` with `[0, 1, 2]` as input.
//! Since this is a pure function call, the only objects needed are the
//! framework packages (0x1 move-stdlib, 0x2 iota-framework).
//!
//! Usage:
//!   cargo run --example offline_dev_inspect

use anyhow::Result;
use iota_framework::BuiltInFramework;
use iota_local_executor::{InMemoryStore, OfflineExecutor};
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    effects::TransactionEffectsAPI,
    transaction::{TransactionData, TransactionDataAPI},
};

fn main() -> Result<()> {
    // Build the in-memory store with all framework packages.
    // These are compiled into the binary — no network access needed.
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        println!("  Loading framework package: {}", obj.id());
        store.insert(obj);
    }

    // Create the offline executor with reasonable defaults.
    // These values don't need to match any real network state for dev-inspect.
    let executor = OfflineExecutor::new(
        ProtocolVersion::MAX,
        1000, // reference_gas_price (arbitrary for dev-inspect)
        0,    // epoch_id
        0,    // epoch_timestamp_ms
        store,
    )?;

    // Base64-encoded transaction bytes for: 0x2::hash::blake2b256([0, 1, 2])
    let tx_bytes_base64 = "AAABAAQDAAECAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgRoYXNoCmJsYWtlMmIyNTYAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6AMAAAAAAAAAypo7AAAAAAA=";

    // Decode the transaction
    let tx_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_base64)?;
    let transaction: TransactionData = bcs::from_bytes(&tx_bytes)?;

    println!("\nExecuting transaction OFFLINE (no network access)...");
    println!("  Sender: {}", transaction.sender());

    // Execute fully offline
    let result = executor.dev_inspect(transaction)?;

    // Check for execution errors
    if let Err(ref err) = result.execution_result {
        eprintln!("\nExecution failed: {err}");
        println!("  Effects status: {:?}", result.effects.status());
        return Ok(());
    }

    println!("\nExecution succeeded!");
    println!("  Effects status: {:?}", result.effects.status());

    if let Ok(ref results) = result.execution_result {
        println!("  Command results: {}", results.len());
        for (i, (mutable_ref_outputs, return_values)) in results.iter().enumerate() {
            println!(
                "    [{i}] mutable_reference_outputs: {}, return_values: {}",
                mutable_ref_outputs.len(),
                return_values.len()
            );
            for (j, (bytes, type_tag)) in return_values.iter().enumerate() {
                println!("      return[{j}]: type={type_tag}, bytes={bytes:?}");
            }
        }
    }

    Ok(())
}
