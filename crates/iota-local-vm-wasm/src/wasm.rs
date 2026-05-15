// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `wasm-bindgen` surface exported to JavaScript.
//!
//! The browser flow is:
//!   1. JS hands us the base-64 BCS-encoded `TransactionData` (and, for signed
//!      simulation, the raw signature blob(s) the wallet holds).
//!   2. We decode and return the set of object IDs the transaction touches. For
//!      a `MoveAuthenticator` signature, JS additionally calls
//!      [`decode_move_authenticator_objects`] to learn which auth-related
//!      objects to fetch (the authenticator's input objects + the
//!      `AuthenticatorFunctionRefV1` dynamic field on the account object).
//!   3. JS fetches those objects (and any dynamic-field children) via the
//!      node's GraphQL/JSON-RPC, base-64-encoding the resulting BCS objects.
//!   4. JS calls [`simulate`] with the chain info + objects + (optionally)
//!      signatures. The wasm side runs them through the local Move VM and
//!      returns a structured result.

use base64::Engine;
use fastcrypto::traits::ToFromBytes;
use iota_protocol_config::ProtocolVersion;
use iota_types::{
    base_types::ObjectID,
    dynamic_field::{DynamicFieldInfo, derive_dynamic_field_id},
    effects::TransactionEffectsAPI,
    object::Object,
    parse_iota_type_tag,
    signature::GenericSignature,
    transaction::{SenderSignedData, TransactionData, TransactionDataAPI},
    transaction_executor::VmChecks,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crate::{
    executor::{OfflineExecutor, auth_function_field_id},
    store::InMemoryStore,
};

#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

fn b64_decode(s: &str) -> Result<Vec<u8>, JsError> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| JsError::new(&format!("base64 decode: {e}")))
}

fn err_to_js<E: std::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

/// Result of decoding a transaction: lists the object IDs the JS side must
/// fetch and ship back as inputs.
#[derive(Serialize, Deserialize)]
pub struct DecodedTransaction {
    pub sender: String,
    pub gas_budget: u64,
    pub gas_price: u64,
    /// All distinct object IDs the transaction references: explicit inputs,
    /// gas payments, and receiving objects. Returned in hex form.
    pub object_ids: Vec<String>,
    /// Subset of `object_ids` that are shared objects — the JS side should
    /// also walk these for dynamic-field children.
    pub shared_object_ids: Vec<String>,
    pub kind_summary: String,
}

#[wasm_bindgen]
pub fn decode_transaction(tx_b64: &str) -> Result<JsValue, JsError> {
    let bytes = b64_decode(tx_b64)?;
    let tx: TransactionData = bcs::from_bytes(&bytes)
        .map_err(|e| JsError::new(&format!("bcs decode TransactionData: {e}")))?;

    let input_object_kinds = tx.input_objects().map_err(err_to_js)?;

    let mut ids: Vec<String> = Vec::new();
    let mut shared: Vec<String> = Vec::new();
    use iota_types::transaction::InputObjectKind;
    for kind in &input_object_kinds {
        let id = kind.object_id().to_hex();
        if matches!(kind, InputObjectKind::SharedMoveObject { .. }) {
            shared.push(id.clone());
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for gas in tx.gas() {
        let id = gas.object_id.to_hex();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for r in &tx.receiving_objects() {
        let id = r.object_id.to_hex();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.sort();
    ids.dedup();

    let kind_summary = format!("{:?}", tx.kind());

    let out = DecodedTransaction {
        sender: tx.sender().to_string(),
        gas_budget: tx.gas_budget(),
        gas_price: tx.gas_price(),
        object_ids: ids,
        shared_object_ids: shared,
        kind_summary,
    };
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsError::new(&e.to_string()))
}

/// Derive the on-chain ID of a `Field<K, V>` wrapper object from its parent
/// ID and name. The IOTA GraphQL `dynamicFields` connection exposes
/// `name { type { repr }, bcs }` but not the field-object's address, so the
/// JS side calls this to walk the dynamic-field tree itself. Mirrors
/// `iota_types::dynamic_field::derive_dynamic_field_id`.
#[wasm_bindgen]
pub fn derive_field_id(
    parent_id_hex: &str,
    key_type_repr: &str,
    key_bcs_b64: &str,
    is_dynamic_object_field: bool,
) -> Result<String, JsError> {
    let parent: ObjectID = parent_id_hex
        .parse()
        .map_err(|e| JsError::new(&format!("parent id: {e}")))?;
    let key_bytes = b64_decode(key_bcs_b64)?;
    let key_type =
        parse_iota_type_tag(key_type_repr).map_err(|e| JsError::new(&format!("key type: {e}")))?;
    let wrapper_type = if is_dynamic_object_field {
        DynamicFieldInfo::dynamic_object_field_wrapper(key_type).into()
    } else {
        key_type
    };
    let id = derive_dynamic_field_id(parent, &wrapper_type, &key_bytes)
        .map_err(|e| JsError::new(&format!("derive: {e}")))?;
    Ok(id.to_hex())
}

/// Auth-related object IDs the JS side must fetch to verify a
/// `MoveAuthenticator` signature. `null` for non-`MoveAuthenticator`
/// signatures.
#[derive(Serialize, Deserialize)]
pub struct MoveAuthenticatorObjects {
    /// Object IDs the authenticator function takes as inputs — the account
    /// object plus any `signature_args` the authenticator points at.
    pub input_object_ids: Vec<String>,
    /// The on-chain abstract-account object ID.
    pub account_object_id: String,
    /// The dynamic field on the account object that stores the
    /// `AuthenticatorFunctionRefV1` — JS must fetch this object too, the
    /// VM needs it to know which Move function to call for authentication.
    pub auth_function_field_id: String,
}

/// Decode a single base-64-encoded raw signature blob. If it is a
/// `MoveAuthenticator`, returns the auth-related object IDs JS must fetch
/// before calling [`simulate`]. Returns `null` for any other signature
/// scheme — for those, no extra objects are needed.
#[wasm_bindgen]
pub fn decode_move_authenticator_objects(sig_b64: &str) -> Result<JsValue, JsError> {
    let bytes = b64_decode(sig_b64)?;
    let sig = GenericSignature::from_bytes(&bytes)
        .map_err(|e| JsError::new(&format!("decode signature: {e}")))?;

    let GenericSignature::MoveAuthenticator(auth) = sig else {
        return Ok(JsValue::NULL);
    };

    let (account_id, _, _) = auth
        .object_to_authenticate_components()
        .map_err(|e| JsError::new(&format!("object_to_authenticate: {e}")))?;

    let input_object_ids: Vec<String> = auth
        .input_objects()
        .iter()
        .map(|kind| kind.object_id().to_hex())
        .collect();

    let field_id = auth_function_field_id(account_id).map_err(err_to_js)?;

    let out = MoveAuthenticatorObjects {
        input_object_ids,
        account_object_id: account_id.to_hex(),
        auth_function_field_id: field_id.to_hex(),
    };
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsError::new(&e.to_string()))
}

/// Input objects supplied by the JS side. `bcs_b64` is the result of
/// `bcs::to_bytes(&iota_types::object::Object)` then base-64. JSON-RPC
/// callers can build this from `iota_getObject` responses (the SDK does
/// the BCS encoding).
#[derive(Serialize, Deserialize)]
pub struct BcsObject {
    pub bcs_b64: String,
}

#[derive(Serialize, Deserialize)]
pub struct SimulateRequest {
    pub tx_b64: String,
    pub protocol_version: u64,
    pub reference_gas_price: u64,
    pub epoch_id: u64,
    pub epoch_timestamp_ms: u64,
    pub objects: Vec<BcsObject>,
    /// When true, run with full sign-time checks. When false, use dev-inspect
    /// (relaxed) mode — matches `iota_local_executor::VmChecks::Disabled`.
    pub strict: bool,
    /// Optional: raw signature blobs (base-64-encoded). When present, the
    /// simulator verifies signatures before executing — cryptographically for
    /// standard schemes (Ed25519, Secp256k1, Secp256r1, MultiSig, Passkey),
    /// and via the Move VM for `MoveAuthenticator` (the authenticator's
    /// function is invoked as part of execution).
    ///
    /// Each entry is the raw `[flag || …]` blob the wallet holds, base-64
    /// encoded. Verification failure surfaces as a thrown error before any
    /// execution effects are computed.
    #[serde(default)]
    pub signatures: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SimulateResult {
    pub success: bool,
    pub status: String,
    pub gas_used: u64,
    pub computation_cost: u64,
    pub storage_cost: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
    pub mutated_count: usize,
    pub created_count: usize,
    pub deleted_count: usize,
    pub event_count: usize,
    pub events: Vec<EventSummary>,
    pub error: Option<String>,
    /// For each PTB command, the count of mutable-ref outputs / return values.
    pub command_results: Vec<CommandResultSummary>,
    /// `true` when [`SimulateRequest::signatures`] was non-empty and signature
    /// verification + (for `MoveAuthenticator`) the authenticator function
    /// all succeeded. `false` otherwise.
    ///
    /// For standard schemes this means the crypto check passed before
    /// execution (a failure would have surfaced as a thrown error and you
    /// wouldn't be reading this struct). For `MoveAuthenticator` it means
    /// the authenticator function did *not* abort during execution.
    pub signature_verified: bool,
}

#[derive(Serialize, Deserialize)]
pub struct EventSummary {
    pub package: String,
    pub module: String,
    pub name: String,
    pub sender: String,
}

#[derive(Serialize, Deserialize)]
pub struct CommandResultSummary {
    pub mutable_ref_outputs: usize,
    pub return_values: usize,
}

#[wasm_bindgen]
pub fn simulate(req: JsValue) -> Result<JsValue, JsError> {
    let req: SimulateRequest =
        serde_wasm_bindgen::from_value(req).map_err(|e| JsError::new(&e.to_string()))?;

    let tx_bytes = b64_decode(&req.tx_b64)?;
    let tx: TransactionData =
        bcs::from_bytes(&tx_bytes).map_err(|e| JsError::new(&format!("bcs decode tx: {e}")))?;

    let mut store = InMemoryStore::with_framework();
    web_sys::console::log_1(
        &format!("[wasm] loading {} caller-supplied objects", req.objects.len()).into(),
    );
    for (i, o) in req.objects.iter().enumerate() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&o.bcs_b64)
            .map_err(|e| JsError::new(&format!("object[{i}] base64: {e}")))?;
        let obj: Object =
            bcs::from_bytes(&bytes).map_err(|e| JsError::new(&format!("object[{i}] bcs: {e}")))?;
        web_sys::console::log_1(
            &format!("[wasm] object[{i}] id={}", obj.id().to_hex()).into(),
        );
        store.insert(obj);
    }

    let executor = OfflineExecutor::new(
        ProtocolVersion::new(req.protocol_version),
        req.reference_gas_price,
        req.epoch_id,
        req.epoch_timestamp_ms,
        store,
    )
    .map_err(|e| JsError::new(&format!("executor init: {e}")))?;

    let checks = if req.strict {
        VmChecks::Enabled
    } else {
        VmChecks::Disabled
    };

    let signed = !req.signatures.is_empty();
    let result = if signed {
        let mut sigs: Vec<GenericSignature> = Vec::with_capacity(req.signatures.len());
        for (i, s) in req.signatures.iter().enumerate() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| JsError::new(&format!("signature[{i}] base64: {e}")))?;
            let sig = GenericSignature::from_bytes(&bytes)
                .map_err(|e| JsError::new(&format!("signature[{i}] decode: {e}")))?;
            sigs.push(sig);
        }
        let signed_data = SenderSignedData::new(tx, sigs);
        executor
            .simulate_signed_transaction(signed_data, checks)
            .map_err(|e| JsError::new(&format!("simulate: {e}")))?
    } else {
        executor
            .simulate_transaction(tx, checks)
            .map_err(|e| JsError::new(&format!("simulate: {e}")))?
    };

    let status = result.effects.status();
    let success = status.is_success();
    let gas = result.effects.gas_cost_summary();

    let (created, mutated, deleted) = (
        result.effects.created().len(),
        result.effects.mutated().len(),
        result.effects.deleted().len(),
    );

    let events: Vec<EventSummary> = result
        .events
        .as_ref()
        .map(|e| {
            e.data
                .iter()
                .map(|ev| EventSummary {
                    package: ev.package_id.to_hex(),
                    module: ev.module.to_string(),
                    name: ev.type_.name().to_string(),
                    sender: ev.sender.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let command_results = match &result.execution_result {
        Ok(per_cmd) => per_cmd
            .iter()
            .map(|(mut_refs, ret_vals)| CommandResultSummary {
                mutable_ref_outputs: mut_refs.len(),
                return_values: ret_vals.len(),
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let error = result
        .execution_result
        .as_ref()
        .err()
        .map(|e| e.to_string());

    // For signed simulation, the crypto checks have already passed by this
    // point (a failure would have returned a JsError above). For a
    // `MoveAuthenticator`, the auth function's verdict shows up as the
    // transaction's overall execution status — so we treat overall success
    // as "signature accepted".
    let signature_verified = signed && success;

    let out = SimulateResult {
        success,
        status: format!("{status:?}"),
        gas_used: gas.gas_used(),
        computation_cost: gas.computation_cost,
        storage_cost: gas.storage_cost,
        storage_rebate: gas.storage_rebate,
        non_refundable_storage_fee: gas.non_refundable_storage_fee,
        mutated_count: mutated,
        created_count: created,
        deleted_count: deleted,
        event_count: events.len(),
        events,
        error,
        command_results,
        signature_verified,
    };

    serde_wasm_bindgen::to_value(&out).map_err(|e| JsError::new(&e.to_string()))
}
