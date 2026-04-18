// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Debug / tracing / gas-profiling configuration and result types for the local
//! executor.
//!
//! See [`crate::debug::DebugConfig`] for the user-facing configuration surface
//! and [`crate::debug::DebugSimulateResult`] for the additive result shape that
//! carries captured artifacts alongside the normal
//! [`SimulateTransactionResult`](iota_types::transaction_executor::SimulateTransactionResult).
//!
//! Design and phased rollout live in `docs/LOCAL_DEBUGGING.md`.

use std::path::PathBuf;

use iota_types::transaction_executor::SimulateTransactionResult;
use move_trace_format::format::MoveTrace;

/// User-facing configuration for a single debug-enabled simulation run.
///
/// Every field is independently toggleable. The default value disables all
/// debug capture and matches the behavior of the non-debug simulate methods.
#[derive(Default, Clone)]
pub struct DebugConfig {
    /// Enable Move `debug::print` output.
    ///
    /// Flips the `silent` flag that the local executor passes to
    /// `iota_execution::executor()`. In Phase 1, prints go to stdout; in
    /// Phase 2, with [`DebugConfig::structured_debug_capture`] set, they are
    /// captured into [`DebugArtifacts::debug_prints`] instead.
    pub capture_debug_prints: bool,

    /// Enable the Move VM gas profiler and choose where the Speedscope JSON
    /// ends up.
    pub profile: Option<ProfileSink>,

    /// Enable instruction-level execution tracing via
    /// [`move_trace_format::format::MoveTraceBuilder`]. The resulting
    /// [`MoveTrace`] is returned in [`DebugArtifacts::trace`].
    pub trace: bool,

    /// Phase 2: route `debug::print` output into an in-memory buffer instead
    /// of stdout. No effect in Phase 1.
    pub structured_debug_capture: bool,
}

/// Where to write the Speedscope-format gas profile.
#[derive(Clone)]
pub enum ProfileSink {
    /// Write the profile JSON to the given path on disk (forwarded directly
    /// to the Move VM profiler).
    File(PathBuf),
    /// Write the profile to a temporary file and read its bytes back into
    /// [`ProfileOutput::Json`] after execution.
    Capture,
}

impl ProfileSink {
    /// Return the filesystem path the VM profiler should write to. For
    /// [`ProfileSink::File`] that's the caller-supplied path; for
    /// [`ProfileSink::Capture`] the caller is expected to materialise a temp
    /// path at the simulate-site.
    pub fn as_path(&self) -> Option<&PathBuf> {
        match self {
            ProfileSink::File(p) => Some(p),
            ProfileSink::Capture => None,
        }
    }
}

/// The resulting gas profile, either a filesystem path the VM wrote to or the
/// raw JSON bytes read back after execution.
#[derive(Debug)]
pub enum ProfileOutput {
    Path(PathBuf),
    Json(Vec<u8>),
}

/// Captured debug artifacts from a simulation run. All fields are optional —
/// they are populated if and only if the matching [`DebugConfig`] toggle was
/// enabled.
#[derive(Default)]
pub struct DebugArtifacts {
    /// Structured `debug::print` lines. Always empty in Phase 1 (prints go to
    /// stdout). Populated in Phase 2 when
    /// [`DebugConfig::structured_debug_capture`] is set.
    pub debug_prints: Vec<String>,

    /// Gas profile output, if [`DebugConfig::profile`] was set.
    pub profile: Option<ProfileOutput>,

    /// Instruction-level execution trace, if [`DebugConfig::trace`] was set.
    pub trace: Option<MoveTrace>,
}

/// A simulation result extended with captured debug artifacts. The
/// unmodified [`SimulateTransactionResult`] is preserved verbatim so callers
/// that read effects / events / objects don't change.
pub struct DebugSimulateResult {
    pub result: SimulateTransactionResult,
    pub artifacts: DebugArtifacts,
}
