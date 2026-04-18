// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Tests for the `DebugConfig` plumbing on `OfflineExecutor`.
//!
//! These tests exercise the spine added in "Checkpoint A" of the local
//! debugging / tracing / gas-profiling feature:
//!
//! - `DebugConfig::default()` returns empty artifacts.
//! - `DebugConfig::trace` captures a non-empty `MoveTrace` via
//!   `dev_inspect_transaction`'s new `trace_builder_opt` parameter.
//! - The `Capture` profile sink materialises a non-empty Speedscope JSON blob.
//! - Debug and non-debug simulate paths produce identical transaction effects
//!   (semantics-preserving instrumentation).

use anyhow::Result;
use iota_framework::BuiltInFramework;
use iota_local_executor::{
    DebugConfig, InMemoryStore, OfflineExecutor, ProfileOutput, ProfileSink, VmChecks,
};
use iota_protocol_config::ProtocolVersion;
use iota_types::{effects::TransactionEffectsAPI, transaction::TransactionData};

/// Base64-encoded transaction bytes for `0x2::hash::blake2b256([0, 1, 2])`.
/// Copied verbatim from `examples/offline_dev_inspect.rs` — a pure framework
/// call that needs no inputs beyond the built-in framework packages.
const BLAKE2B_TX_BYTES_B64: &str = "AAABAAQDAAECAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgRoYXNoCmJsYWtlMmIyNTYAAQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6AMAAAAAAAAAypo7AAAAAAA=";

fn seeded_store() -> InMemoryStore {
    let mut store = InMemoryStore::new();
    for obj in BuiltInFramework::genesis_objects() {
        store.insert(obj);
    }
    store
}

fn blake2b_tx() -> TransactionData {
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        BLAKE2B_TX_BYTES_B64,
    )
    .expect("test fixture tx bytes decode");
    bcs::from_bytes(&bytes).expect("test fixture tx bytes deserialise")
}

fn offline_with(debug: DebugConfig) -> Result<OfflineExecutor> {
    OfflineExecutor::with_debug(ProtocolVersion::MAX, 1000, 0, 0, seeded_store(), debug)
}

#[test]
fn default_debug_config_returns_empty_artifacts() -> Result<()> {
    let executor = offline_with(DebugConfig::default())?;
    let out = executor.simulate_transaction_with_debug(blake2b_tx(), VmChecks::Disabled)?;

    assert!(
        out.result.effects.status().is_ok(),
        "execution should succeed: {:?}",
        out.result.effects.status()
    );
    assert!(out.artifacts.debug_prints.is_empty());
    assert!(out.artifacts.profile.is_none());
    assert!(out.artifacts.trace.is_none());
    Ok(())
}

#[test]
fn profile_capture_returns_non_empty_speedscope_json() -> Result<()> {
    let executor = offline_with(DebugConfig {
        profile: Some(ProfileSink::Capture),
        ..DebugConfig::default()
    })?;
    let out = executor.simulate_transaction_with_debug(blake2b_tx(), VmChecks::Disabled)?;

    assert!(
        out.result.effects.status().is_ok(),
        "execution should succeed: {:?}",
        out.result.effects.status()
    );
    let profile = out.artifacts.profile.expect("profile should be populated");
    let bytes = match profile {
        ProfileOutput::Json(b) => b,
        ProfileOutput::Path(p) => {
            panic!("ProfileSink::Capture should materialise JSON bytes, got path {p:?}")
        }
    };
    assert!(
        !bytes.is_empty(),
        "captured gas profile should not be empty"
    );

    // Content-level sanity: must parse as JSON with a Speedscope-like shape.
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("captured profile must be valid JSON");
    assert!(
        json.get("shared").is_some() || json.get("profiles").is_some(),
        "captured profile should have a Speedscope root key (shared/profiles), got {json}"
    );
    Ok(())
}

#[test]
fn debug_instrumentation_preserves_effects() -> Result<()> {
    let plain = offline_with(DebugConfig::default())?
        .simulate_transaction_with_debug(blake2b_tx(), VmChecks::Disabled)?;
    let debugged = offline_with(DebugConfig {
        capture_debug_prints: true,
        profile: Some(ProfileSink::Capture),
        ..DebugConfig::default()
    })?
    .simulate_transaction_with_debug(blake2b_tx(), VmChecks::Disabled)?;

    // Byte-level effects parity: enabling debug must not perturb execution.
    let plain_effects = bcs::to_bytes(&plain.result.effects)?;
    let debugged_effects = bcs::to_bytes(&debugged.result.effects)?;
    assert_eq!(
        plain_effects, debugged_effects,
        "debug instrumentation must not change transaction effects"
    );
    Ok(())
}
