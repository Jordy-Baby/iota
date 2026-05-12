// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! `iota-local-exec` — a thin CLI wrapper over `iota-local-executor` for
//! running single `MoveCall` PTBs against locally-compiled Move packages.
//!
//! This is Option C from the CLI design discussion: minimal surface, no
//! dependency on the `iota` binary crate or its `client_ptb` parser. Multi-
//! command PTBs (split-coins, merge-coins, dotted result access, named args)
//! are deliberately **not** supported — for those, use the crate's Rust API
//! directly in a test or script.
//!
//! See `README.md` for the full flag surface and limitations.

mod args;
mod assertions;
mod auth;
mod call;
mod output;
mod packages;
mod ptb;

use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use iota_local_executor::{
    DebugConfig, DebugSimulateResult, OfflineExecutor, ProfileSink, VmChecks,
};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};

use crate::{
    args::parse_address,
    output::{ExecutionSummary, build_summary_json, write_artifacts, write_summary_json},
};

const DEFAULT_GAS_BUDGET: u64 = 100_000_000;
const DEFAULT_GAS_PRICE: u64 = 1000;

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

pub(crate) const EXIT_OK: u8 = 0;
pub(crate) const EXIT_ASSERTION_FAILED: u8 = 1;
const EXIT_COMPILE_ERROR: u8 = 2;
const EXIT_RUNTIME_ERROR: u8 = 3;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    DevInspect,
    DryRun,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StdoutFormat {
    Json,
    Pretty,
    None,
}

#[derive(Parser, Debug)]
#[command(
    name = "iota-local-exec",
    about = "Run a single MoveCall PTB locally against compiled Move packages.",
    long_about = "Simulate a single MoveCall PTB against one or more locally-compiled Move \
                  packages, with optional gas profiling, instruction tracing, and \
                  debug::print capture. Multi-command PTBs are not supported in v1 — use \
                  the `iota-local-executor` Rust API directly for complex scenarios.",
    version
)]
struct Cli {
    // --- Packages ---
    /// Local Move package to compile, in the form `PATH` or `PATH=ID`. The
    /// package's declared named address is rebound to `ID` (or
    /// 0x42, 0x43, ... auto-assigned in order). Repeatable.
    #[arg(long = "package", value_name = "PATH[=ID]")]
    packages: Vec<String>,

    /// Override a named address: `KEY=ID`. Defaults to the package name
    /// declared in the first `--package`. Repeatable.
    #[arg(long = "named-address", value_name = "KEY=ID")]
    named_addresses: Vec<String>,

    // --- Transaction body ---
    /// Move function to call: `PKG::MODULE::FN`. `PKG` may be omitted if
    /// exactly one local package is supplied.
    #[arg(long = "call", value_name = "PKG::MODULE::FN")]
    call: Option<String>,

    /// A typed argument, e.g. `10u64`, `true`, `@0xADDR`, `"text"`, or
    /// `object(0xID)`. Repeatable.
    #[arg(long = "arg", value_name = "VALUE")]
    args: Vec<String>,

    /// Type argument as a type tag (e.g. `0x2::iota::IOTA`). Repeatable.
    #[arg(long = "type-arg", value_name = "TYPE")]
    type_args: Vec<String>,

    // --- Sender & gas ---
    /// Sender address. Defaults to 0x0.
    #[arg(long, value_name = "0xADDR", default_value = "0x0")]
    sender: String,

    /// Gas budget in nanos.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_GAS_BUDGET)]
    gas_budget: u64,

    /// Gas price.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_GAS_PRICE)]
    gas_price: u64,

    /// Gas payment object ID. When omitted, the executor mocks a gas coin.
    #[arg(long, value_name = "0xID")]
    gas_coin: Option<String>,

    // --- Authenticator ---
    /// MoveAuthenticator function: `PKG::MODULE::FN`.
    #[arg(long = "authenticator", value_name = "PKG::MODULE::FN")]
    authenticator: Option<String>,

    /// Authenticator-function argument. Same grammar as `--arg`. Repeatable.
    #[arg(long = "auth-arg", value_name = "VALUE")]
    auth_args: Vec<String>,

    /// Account object ID that the authenticator authenticates against.
    #[arg(long = "account-object", value_name = "0xID")]
    account_object: Option<String>,

    // --- Simulation ---
    /// Simulation mode.
    #[arg(long, value_enum, default_value_t = ModeArg::DevInspect)]
    mode: ModeArg,

    /// Disable gas profiling (Speedscope). Enabled by default.
    #[arg(long)]
    no_profile: bool,

    /// Disable instruction tracing. Enabled by default.
    #[arg(long)]
    no_trace: bool,

    /// Disable `debug::print` capture. Enabled by default.
    #[arg(long)]
    no_debug_prints: bool,

    // --- Remote state (v2) ---
    /// Prefetch remote objects from this JSON-RPC endpoint.
    /// **Not supported in v1.**
    #[arg(long, value_name = "URL")]
    remote_rpc: Option<String>,

    /// Remote object to prefetch: `0xID` or `0xID@VER`. **Not supported in
    /// v1.**
    #[arg(long = "remote-object", value_name = "0xID[@VER]")]
    remote_objects: Vec<String>,

    // --- Assertions ---
    /// Fail unless the simulated transaction succeeds.
    #[arg(long)]
    require_success: bool,

    /// Fail if the net gas usage is greater than or equal to `N`.
    #[arg(long, value_name = "N")]
    assert_gas_below: Option<u64>,

    /// Fail if the per-function gas for `module::function` is greater than or
    /// equal to `N`. Repeatable.
    #[arg(long = "assert-function-gas-below", value_name = "FN=N")]
    assert_function_gas_below: Vec<String>,

    // --- Output ---
    /// Write artifacts into this directory. Directory is created if missing.
    #[arg(long, value_name = "PATH")]
    out_dir: Option<PathBuf>,

    /// Top-level summary format on stdout.
    #[arg(long, value_enum, default_value_t = StdoutFormat::Json)]
    stdout: StdoutFormat,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            if err.downcast_ref::<CompileFail>().is_some() {
                ExitCode::from(EXIT_COMPILE_ERROR)
            } else {
                ExitCode::from(EXIT_RUNTIME_ERROR)
            }
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    // --- Preflight: reject v2-only flags ---
    if cli.remote_rpc.is_some() || !cli.remote_objects.is_empty() {
        bail!(
            "remote state fetching (--remote-rpc / --remote-object) is not supported in v1 — \
             use the iota-local-executor Rust API directly"
        );
    }

    // --- Parse packages & build store ---
    let protocol_config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);
    let packages::LoadedPackages {
        packages,
        store,
        aliases,
    } = packages::load(&cli.packages, &cli.named_addresses, &protocol_config)?;

    // --- Resolve --call and function signature ---
    let call_spec = cli
        .call
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--call PKG::MODULE::FN is required"))?;
    let call = call::resolve_call_spec(call_spec, &packages, &aliases)?;
    let pkg = packages
        .iter()
        .find(|p| p.id() == call.package_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--call references package {} but no such --package was supplied",
                call.package_id
            )
        })?;
    let module = call::find_module_in_package(pkg, &call.module)?;
    let fn_info = call::resolve_function(module, &call.function)?;

    // --- Build PTB / transaction ---
    let sender = parse_address(&cli.sender)
        .map_err(|e| anyhow::anyhow!("parsing --sender `{}`: {e}", cli.sender))?;
    let tx = ptb::build(
        &call,
        &fn_info,
        &cli.args,
        &cli.type_args,
        &store,
        sender,
        cli.gas_coin.as_deref(),
        cli.gas_budget,
        cli.gas_price,
    )?;

    // --- Optional authenticator ---
    let signed = if let Some(auth_spec) = &cli.authenticator {
        Some(auth::wrap(
            tx.clone(),
            auth_spec,
            &cli.auth_args,
            &cli.account_object,
            &packages,
            &aliases,
            &store,
        )?)
    } else {
        None
    };

    // --- Execute ---
    let debug_config = DebugConfig {
        capture_debug_prints: !cli.no_debug_prints,
        profile: if cli.no_profile {
            None
        } else {
            Some(ProfileSink::Capture)
        },
        trace: !cli.no_trace,
        structured_debug_capture: !cli.no_debug_prints,
    };
    let checks = match cli.mode {
        ModeArg::DevInspect => VmChecks::Disabled,
        ModeArg::DryRun => VmChecks::Enabled,
    };

    let executor = OfflineExecutor::with_debug(
        ProtocolVersion::MAX,
        cli.gas_price,
        0,
        0,
        store,
        debug_config,
    )
    .map_err(|e| anyhow::Error::new(RuntimeFail(e)))?;

    let out: DebugSimulateResult = match signed {
        Some(signed) => executor
            .simulate_signed_transaction_with_debug(signed, checks)
            .map_err(|e| anyhow::Error::new(RuntimeFail(e)))?,
        None => executor
            .simulate_transaction_with_debug(tx, checks)
            .map_err(|e| anyhow::Error::new(RuntimeFail(e)))?,
    };

    // --- Summarise, assert, write artifacts ---
    let summary = ExecutionSummary::from_result(&out);
    let assertion_rows = assertions::evaluate(
        &summary,
        cli.require_success,
        cli.assert_gas_below,
        &cli.assert_function_gas_below,
    )?;

    let mut artifact_paths: BTreeMap<String, String> = BTreeMap::new();
    if let Some(dir) = &cli.out_dir {
        write_artifacts(dir, &out, &summary.function_gas_rows, &mut artifact_paths)?;
    }

    let summary_json = build_summary_json(&summary, &assertion_rows, &artifact_paths);
    if let Some(dir) = &cli.out_dir {
        write_summary_json(dir, &summary_json)?;
    }

    // --- stdout ---
    match cli.stdout {
        StdoutFormat::Json => {
            println!("{}", serde_json::to_string(&summary_json)?);
        }
        StdoutFormat::Pretty => {
            println!("{}", serde_json::to_string_pretty(&summary_json)?);
        }
        StdoutFormat::None => {}
    }

    Ok(assertions::exit_code(
        summary.status_ok,
        cli.require_success,
        &assertion_rows,
    ))
}

// ---------------------------------------------------------------------------
// Error wrappers used purely to drive the exit code.
// ---------------------------------------------------------------------------

/// Marker for compile-time failures so `main` can pick exit code 2. Wrapping
/// an `anyhow::Error` in this struct threads the classification through the
/// `anyhow::Error` chain; `run`'s return value is downcast inside `main` to
/// resolve the exit code.
#[derive(Debug)]
pub(crate) struct CompileFail(pub(crate) anyhow::Error);
impl std::fmt::Display for CompileFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compile error: {:#}", self.0)
    }
}
impl std::error::Error for CompileFail {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Marker for runtime failures (executor / fetch) so `main` can pick exit
/// code 3.
#[derive(Debug)]
struct RuntimeFail(anyhow::Error);
impl std::fmt::Display for RuntimeFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error: {:#}", self.0)
    }
}
impl std::error::Error for RuntimeFail {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}
