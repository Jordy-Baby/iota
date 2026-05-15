// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Replay the committed `web/samples/*.json` fixtures against the same
//! `OfflineExecutor` the wasm bundle calls — proves the demo would succeed
//! before we ship any browser code. Native-only; the test files live in
//! `web/samples/` so the same JSON drives both this test and the browser
//! demo.

use std::{fs, path::PathBuf};

use base64::Engine;
use iota_local_vm_wasm::{InMemoryStore, OfflineExecutor};
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    effects::TransactionEffectsAPI,
    object::Object,
    signature::GenericSignature,
    transaction::{SenderSignedData, TransactionData},
    transaction_executor::VmChecks,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    protocol_version: u64,
    reference_gas_price: u64,
    epoch_id: u64,
    epoch_timestamp_ms: u64,
    tx_b64: String,
    signatures: Vec<String>,
    objects: Vec<FixObj>,
}

#[derive(Deserialize)]
struct FixObj {
    id_hex: String,
    bcs_b64: String,
}

fn load(name: &str) -> Fixture {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "web",
        "samples",
        &format!("{name}.json"),
    ]
    .iter()
    .collect();
    let raw = fs::read_to_string(&path).expect("read fixture");
    serde_json::from_str(&raw).expect("parse fixture")
}

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// Replay a fixture and return the simulation status. Mirrors exactly what
/// `wasm.rs::simulate` does for a signed request.
fn run(name: &str) -> (bool, String) {
    let f = load(name);
    eprintln!("loaded fixture {name}: {} objects", f.objects.len());
    for o in &f.objects {
        eprintln!("  {}", o.id_hex);
    }

    let tx: TransactionData = bcs::from_bytes(&b64(&f.tx_b64)).expect("bcs tx");
    let mut store = InMemoryStore::with_framework();
    for o in &f.objects {
        let obj: Object = bcs::from_bytes(&b64(&o.bcs_b64)).expect("bcs object");
        let decoded_id = obj.id().to_hex();
        let expected_id = o.id_hex.trim_start_matches("0x").to_owned();
        let decoded_short = decoded_id.trim_start_matches("0x").to_owned();
        assert!(
            decoded_short == expected_id || decoded_short == o.id_hex,
            "fixture id_hex {} does not match decoded obj.id() {}",
            o.id_hex,
            decoded_id,
        );
        store.insert(obj);
    }

    use iota_types::base_types::ObjectID;
    use iota_types::transaction::TransactionDataAPI;
    eprintln!("--- tx kinds: ---");
    for k in tx.input_objects().expect("input_objects") {
        let id: ObjectID = k.object_id();
        let present = store.get_object(&id).is_some();
        eprintln!("  {:?} → {} (in store: {})", k, id.to_hex(), present);
    }

    let mut sigs: Vec<GenericSignature> = Vec::new();
    for s in &f.signatures {
        use fastcrypto::traits::ToFromBytes;
        sigs.push(GenericSignature::from_bytes(&b64(s)).expect("sig decode"));
    }
    let signed = SenderSignedData::new(tx, sigs);

    let executor = OfflineExecutor::new(
        ProtocolVersion::new(f.protocol_version),
        f.reference_gas_price,
        f.epoch_id,
        f.epoch_timestamp_ms,
        store,
    )
    .expect("executor");

    let result = executor
        .simulate_signed_transaction(signed, VmChecks::Disabled)
        .expect("simulate_signed_transaction returns Ok");

    let status = result.effects.status().clone();
    let success = status.is_success();
    (success, format!("{status:?}"))
}

#[test]
fn replay_move_auth_free_access_valid() {
    let (success, status) = run("move_auth_free_access_valid");
    assert!(success, "expected success, got: {status}");
}

#[test]
fn replay_move_auth_ed25519_invalid() {
    let (success, status) = run("move_auth_ed25519_invalid");
    assert!(!success, "expected failure (auth rejects), got success: {status}");
}
