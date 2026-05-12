// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Example: 100% offline staking transaction with hardcoded objects.
//!
//! This example executes a staking transaction with **zero network access**.
//! All required objects are hardcoded as BCS-encoded base64 strings, captured
//! from a previous run of the `offline_stake_inspect` example against devnet.
//!
//! This demonstrates the most extreme offline scenario: the binary contains
//! everything needed to simulate a transaction — framework packages (compiled
//! in), protocol config (built from version number), and all on-chain objects
//! (hardcoded BCS bytes).
//!
//! Usage:
//!   cargo run --example hardcoded_offline_inspect

use anyhow::Result;
use iota_local_executor::{InMemoryStore, OfflineExecutor, VmChecks};
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    effects::TransactionEffectsAPI,
    object::Object,
    transaction::{TransactionData, TransactionDataAPI},
};

/// BCS-encoded objects captured from devnet via `offline_stake_inspect`.
/// These are the non-framework objects needed for the staking transaction.
const HARDCODED_OBJECTS: &[&str] = &[
    // IotaSystemState wrapper (0x5)
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbQ9Jb3RhU3lzdGVtU3RhdGUA7bUdAAAAAAAoAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUCAAAAAAAAAAIBAAAAAAAAACDnXBaBYGV4n+oVXcc8fkcxoJgAMmIf+aT+MAXTib4ChgD95wEAAAAA",
    // Coin 0x19cb... (stake coin)
    "AAFaPxUAAAAAACgZy88P79O/D92/OlvRRj1QkJEGVj8GEgYQYQFNEY90c2Dw3p8SAAAAACIitGaiQ5nrz17A8EgggSriD+oQN8c2z+xgh1OqOLUiIKThrTgId8HneJoUEghQSslQ/DYSHDAyGitMgx3fUkGEsPUOAAAAAAA=",
    // IotaSystemStateInner dynamic field child (0x5b89...)
    "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",
    // Gas coin 0x96da...
    "AAHIEwAAAAAAACiW2s71iGDGWI7p4MjNqhmYFMN9Si0Q3vG1/X+fFYDVJADkC1QCAAAAACIitGaiQ5nrz17A8EgggSriD+oQN8c2z+xgh1OqOLUiIDQPuwYomHcpcaLz1WQ0wprcD/UfSD0o3l1EeWTzV+eisPUOAAAAAAA=",
    // Gas coin 0xcf14...
    "AAHHEwAAAAAAACjPFCwjsZFjUhgajd5ZSwA/VzogxQh/JEQL05VfqazefQDkC1QCAAAAACIitGaiQ5nrz17A8EgggSriD+oQN8c2z+xgh1OqOLUiIP5URDxd6emntyKuM4foZFKT1Cr1q990t+NfCXWvKtdjsPUOAAAAAAA=",
];

/// Epoch parameters captured from devnet at the time of object fetch.
const PROTOCOL_VERSION: u64 = 21;
const REFERENCE_GAS_PRICE: u64 = 1000;
const EPOCH_ID: u64 = 9;
const EPOCH_TIMESTAMP_MS: u64 = 1773246218348;

fn main() -> Result<()> {
    println!("100% offline staking transaction (all objects hardcoded)");
    println!("=========================================================\n");

    // ---------------------------------------------------------------
    // 1. Build the object store from hardcoded BCS + built-in framework
    // ---------------------------------------------------------------
    let mut store = InMemoryStore::with_framework();
    println!("  Loaded built-in framework packages (0x1, 0x2, 0x3, 0x107a, …)");

    // Deserialize hardcoded objects from BCS base64
    for (i, b64) in HARDCODED_OBJECTS.iter().enumerate() {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)?;
        let obj: Object = bcs::from_bytes(&bytes)?;
        println!(
            "  Loaded hardcoded object {} ({})",
            obj.id(),
            if obj.is_package() {
                "package"
            } else {
                "object"
            },
        );
        assert!(
            !bytes.is_empty(),
            "hardcoded object {i} has empty BCS bytes"
        );
        store.insert(obj);
    }
    println!("  Store contains {} objects total\n", store.len());

    // ---------------------------------------------------------------
    // 2. Create the offline executor
    // ---------------------------------------------------------------
    let executor = OfflineExecutor::new(
        ProtocolVersion::new(PROTOCOL_VERSION),
        REFERENCE_GAS_PRICE,
        EPOCH_ID,
        EPOCH_TIMESTAMP_MS,
        store,
    )?;

    // ---------------------------------------------------------------
    // 3. Decode and execute the staking transaction
    // ---------------------------------------------------------------
    let tx_bytes_base64 = "AAADAAgAypo7AAAAAAEBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAUBAAAAAAAAAAEAINqRtZV/6ONntsXV/L9IRp9ACpOV+VnDUxBwOyp4hRr+AgIAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwtpb3RhX3N5c3RlbRFyZXF1ZXN0X2FkZF9zdGFrZQADAQEAAgAAAQIAIiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSIDGcvPD+/Tvw/dvzpb0UY9UJCRBlY/BhIGEGEBTRGPdHNaPxUAAAAAACA8sy5o+BES2kxKEnXM7cH94maytD+8aHx/lXKR0+CK2JbazvWIYMZYjungyM2qGZgUw31KLRDe8bX9f58VgNUkyBMAAAAAAAAgh/y1xeOPt7crDLnGZlW2jTzXSaAXYKtvOEISYwCZb63PFCwjsZFjUhgajd5ZSwA/VzogxQh/JEQL05VfqazefccTAAAAAAAAIJgclRy0Uq4ONHCvw1vi8JqDMorQT11j9eRPTBPeDMA/IiK0ZqJDmevPXsDwSCCBKuIP6hA3xzbP7GCHU6o4tSLoAwAAAAAAAGATQQAAAAAAAA==";

    let tx_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_base64)?;
    let transaction: TransactionData = bcs::from_bytes(&tx_bytes)?;

    println!("Executing staking transaction...");
    println!("  Sender: {}", transaction.sender());
    println!("  Gas budget: {}", transaction.gas_budget());

    let result = executor.simulate_transaction(transaction, VmChecks::Disabled)?;

    // ---------------------------------------------------------------
    // 4. Print results
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
