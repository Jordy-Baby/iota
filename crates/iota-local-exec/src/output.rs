// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Summary schema + per-artifact JSON writers.
//!
//! Owns the on-disk layout under `--out-dir` (`effects.json`, `events.json`,
//! `debug-prints.txt`, `gas-profile.json`, `trace.json`, `function-gas.json`,
//! `summary.json`) and the stdout rendering of the top-level summary.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use iota_local_executor::{DebugSimulateResult, ProfileOutput, summarize_by_function};
use iota_types::{
    effects::{TransactionEffectsAPI, TransactionEvents},
    execution_status::ExecutionStatus,
};
use serde::Serialize;
use serde_json::json;

use crate::assertions::AssertionRow;

/// Per-function gas row for the `function_gas_top` section.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FunctionGasRow {
    pub(crate) module: String,
    pub(crate) function: String,
    pub(crate) gas_used: u64,
    pub(crate) calls: usize,
    pub(crate) is_native: bool,
}

/// A flattened view of a simulation result, ready to be assertion-checked and
/// rendered. Computed once so assertions and the summary share one source of
/// truth.
pub(crate) struct ExecutionSummary {
    pub(crate) status_ok: bool,
    pub(crate) net_gas: i64,
    pub(crate) computation: u64,
    pub(crate) storage: u64,
    pub(crate) rebate: u64,
    pub(crate) events: usize,
    pub(crate) debug_prints: usize,
    pub(crate) function_gas_rows: Vec<FunctionGasRow>,
}

impl ExecutionSummary {
    /// Extract a flat summary from a [`DebugSimulateResult`].
    pub(crate) fn from_result(out: &DebugSimulateResult) -> Self {
        let status_ok = matches!(out.result.effects.status(), ExecutionStatus::Success);
        let gas = out.result.effects.gas_cost_summary().clone();
        let events_count = out
            .result
            .events
            .as_ref()
            .map(|e: &TransactionEvents| e.data.len())
            .unwrap_or(0);
        let debug_prints_count = out.artifacts.debug_prints.len();

        let function_gas_rows = match &out.artifacts.trace {
            Some(trace) => summarize_by_function(trace)
                .into_iter()
                .map(|s| FunctionGasRow {
                    module: s.module.to_string(),
                    function: s.function_name,
                    gas_used: s.gas_used,
                    calls: s.calls,
                    is_native: s.is_native,
                })
                .collect(),
            None => Vec::new(),
        };

        Self {
            status_ok,
            net_gas: gas.net_gas_usage(),
            computation: gas.computation_cost,
            storage: gas.storage_cost,
            rebate: gas.storage_rebate,
            events: events_count,
            debug_prints: debug_prints_count,
            function_gas_rows,
        }
    }

    /// Top-10 per-function gas rows, sorted descending by `gas_used`.
    pub(crate) fn function_gas_top(&self) -> Vec<FunctionGasRow> {
        let mut top = self.function_gas_rows.clone();
        top.sort_by_key(|r| std::cmp::Reverse(r.gas_used));
        top.truncate(10);
        top
    }
}

/// Build the top-level summary JSON object from an [`ExecutionSummary`],
/// assertions, and the map of artifact paths.
pub(crate) fn build_summary_json(
    summary: &ExecutionSummary,
    assertions: &[AssertionRow],
    artifact_paths: &BTreeMap<String, String>,
) -> serde_json::Value {
    json!({
        "status": if summary.status_ok { "success" } else { "failure" },
        "gas": {
            "computation": summary.computation,
            "storage": summary.storage,
            "rebate": summary.rebate,
            "net": summary.net_gas,
        },
        "debug_prints": summary.debug_prints,
        "events": summary.events,
        "function_gas_top": summary.function_gas_top(),
        "assertions": assertions,
        "artifacts": artifact_paths,
    })
}

/// Write every per-file artifact that the simulation produced. Populates
/// `artifact_paths` with the per-kind filenames for the `artifacts` field of
/// the summary.
pub(crate) fn write_artifacts(
    dir: &Path,
    out: &DebugSimulateResult,
    function_gas_rows: &[FunctionGasRow],
    artifact_paths: &mut BTreeMap<String, String>,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating --out-dir `{}`", dir.display()))?;

    // effects.json — always emitted (it's the result).
    let effects_path = dir.join("effects.json");
    std::fs::write(
        &effects_path,
        serde_json::to_vec_pretty(&out.result.effects)?,
    )?;
    artifact_paths.insert("effects".into(), "effects.json".into());

    if let Some(events) = &out.result.events {
        let events_path = dir.join("events.json");
        std::fs::write(&events_path, serde_json::to_vec_pretty(events)?)?;
        artifact_paths.insert("events".into(), "events.json".into());
    }

    if !out.artifacts.debug_prints.is_empty() {
        let prints_path = dir.join("debug-prints.txt");
        std::fs::write(&prints_path, out.artifacts.debug_prints.join("\n"))?;
        artifact_paths.insert("debug_prints".into(), "debug-prints.txt".into());
    }

    if let Some(profile) = &out.artifacts.profile {
        let profile_path = dir.join("gas-profile.json");
        match profile {
            ProfileOutput::Json(bytes) => {
                std::fs::write(&profile_path, bytes)?;
            }
            ProfileOutput::Path(src) => {
                std::fs::copy(src, &profile_path)?;
            }
        }
        artifact_paths.insert("gas_profile".into(), "gas-profile.json".into());
    }

    if let Some(trace) = &out.artifacts.trace {
        let trace_path = dir.join("trace.json");
        std::fs::write(&trace_path, serde_json::to_vec(trace)?)?;
        artifact_paths.insert("trace".into(), "trace.json".into());
    }

    if !function_gas_rows.is_empty() {
        let fg_path = dir.join("function-gas.json");
        std::fs::write(&fg_path, serde_json::to_vec_pretty(function_gas_rows)?)?;
        artifact_paths.insert("function_gas".into(), "function-gas.json".into());
    }

    artifact_paths.insert("summary".into(), "summary.json".into());
    Ok(())
}

/// Persist the top-level `summary.json` file alongside the per-kind artifacts.
pub(crate) fn write_summary_json(dir: &Path, summary: &serde_json::Value) -> Result<()> {
    std::fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(summary)?,
    )?;
    Ok(())
}
