// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for the `iota-local-exec` CLI binary.
//!
//! Drives the real binary (via `CARGO_BIN_EXE_iota-local-exec`) against the
//! `hello_debug` Move fixture in the `iota-local-executor` crate and asserts
//! that:
//! - The process exits 0.
//! - The summary JSON reports `status == "success"` with a non-empty
//!   `function_gas_top` aggregation.
//! - Per-file artifacts (`gas-profile.json`, `trace.json`) are written and
//!   parse back as JSON.

use std::{path::PathBuf, process::Command};

use serde_json::Value;

#[test]
fn cli_runs_hello_debug_fixture() {
    let bin = env!("CARGO_BIN_EXE_iota-local-exec");
    let fixture = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("../iota-local-executor/tests/fixtures/hello_debug");
        p
    };
    let tmp = tempfile::tempdir().expect("tempdir");

    let status = Command::new(bin)
        .args([
            "--package",
            fixture.to_str().unwrap(),
            "--call",
            "hello::sum_to",
            "--arg",
            "10u64",
            "--out-dir",
            tmp.path().to_str().unwrap(),
            "--stdout",
            "none",
        ])
        .status()
        .expect("run iota-local-exec");
    assert!(
        status.success(),
        "iota-local-exec should exit 0, got {status:?}"
    );

    // summary.json
    let summary_bytes = std::fs::read(tmp.path().join("summary.json")).expect("read summary.json");
    let summary: Value = serde_json::from_slice(&summary_bytes).expect("parse summary.json");
    assert_eq!(
        summary.get("status").and_then(|v| v.as_str()),
        Some("success"),
        "summary.json status should be 'success': {summary}"
    );
    let function_gas_top = summary
        .get("function_gas_top")
        .and_then(|v| v.as_array())
        .expect("function_gas_top should be an array");
    assert!(
        !function_gas_top.is_empty(),
        "function_gas_top should be non-empty — got {summary}"
    );

    // gas-profile.json
    let profile_bytes =
        std::fs::read(tmp.path().join("gas-profile.json")).expect("read gas-profile.json");
    let _profile: Value =
        serde_json::from_slice(&profile_bytes).expect("parse gas-profile.json as JSON");

    // trace.json
    let trace_bytes = std::fs::read(tmp.path().join("trace.json")).expect("read trace.json");
    let _trace: Value = serde_json::from_slice(&trace_bytes).expect("parse trace.json as JSON");
}
