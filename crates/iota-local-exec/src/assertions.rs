// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Evaluate `--require-success` / `--assert-gas-below` /
//! `--assert-function-gas-below` against a simulation result and decide the
//! process exit code.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::output::ExecutionSummary;

/// An assertion's name, whether it passed, and the observed value.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssertionRow {
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) observed: u64,
}

/// Evaluate every assertion flag against an [`ExecutionSummary`], returning
/// one [`AssertionRow`] per assertion in the order they appear on the CLI.
pub(crate) fn evaluate(
    summary: &ExecutionSummary,
    require_success: bool,
    assert_gas_below: Option<u64>,
    assert_function_gas_below: &[String],
) -> Result<Vec<AssertionRow>> {
    let mut rows: Vec<AssertionRow> = Vec::new();

    if require_success {
        rows.push(AssertionRow {
            name: "status == success".to_string(),
            passed: summary.status_ok,
            observed: summary.status_ok as u64,
        });
    }

    if let Some(cap) = assert_gas_below {
        let observed = summary.net_gas.max(0) as u64;
        rows.push(AssertionRow {
            name: format!("net_gas < {cap}"),
            passed: observed < cap,
            observed,
        });
    }

    for spec in assert_function_gas_below {
        let (module, function, cap) = parse_function_gas_assertion(spec)?;
        let observed = summary
            .function_gas_rows
            .iter()
            .find(|row| row.module.ends_with(&format!("::{module}")) && row.function == function)
            .map(|row| row.gas_used)
            .unwrap_or(0);
        rows.push(AssertionRow {
            name: format!("{module}::{function} < {cap}"),
            passed: observed < cap,
            observed,
        });
    }

    Ok(rows)
}

/// Decide the final exit code from the simulation status and the evaluated
/// assertion rows. Preserves v1 semantics: a failed tx under
/// `--require-success` and any failing assertion both map to
/// [`crate::EXIT_ASSERTION_FAILED`].
pub(crate) fn exit_code(status_ok: bool, require_success: bool, assertions: &[AssertionRow]) -> u8 {
    if !status_ok && require_success {
        return crate::EXIT_ASSERTION_FAILED;
    }
    if assertions.iter().any(|a| !a.passed) {
        return crate::EXIT_ASSERTION_FAILED;
    }
    crate::EXIT_OK
}

fn parse_function_gas_assertion(spec: &str) -> Result<(String, String, u64)> {
    let (lhs, n) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("--assert-function-gas-below `{spec}` missing `=N`"))?;
    let n: u64 = n
        .parse()
        .with_context(|| format!("parsing gas cap in --assert-function-gas-below `{spec}`"))?;
    let (module, function) = lhs.split_once("::").ok_or_else(|| {
        anyhow!("--assert-function-gas-below `{spec}` left side must be in `module::function` form")
    })?;
    Ok((module.trim().to_string(), function.trim().to_string(), n))
}
