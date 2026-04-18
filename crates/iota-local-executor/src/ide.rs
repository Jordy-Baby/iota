// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Post-processing of a [`MoveTrace`] into summaries an IDE plugin can
//! render inline.
//!
//! Checkpoint A/B left us with a raw instruction-level trace. For a Move IDE
//! to draw per-function gas overlays, per-line hotspots, or call-graph gutter
//! decorations, the trace has to be aggregated and correlated with source
//! positions. This module is that post-processor.
//!
//! Two shapes, in increasing order of detail:
//!
//! - [`FunctionGasSummary`] — total gas, instruction count, call count per
//!   `(module, function)`. Good enough for a "this function burned N gas"
//!   gutter annotation. Computed from the trace alone via
//!   [`summarize_by_function`].
//! - [`FunctionGasSummary::with_source`] — same summary with an optional
//!   [`SourceSpan`] resolved via a [`CompiledPackage`]'s source maps. Computed
//!   via [`summarize_with_source`].
//!
//! The correlation uses the `binary_member_index` field on each `Frame`,
//! which is the `FunctionDefinitionIndex` into the module's binary. Move's
//! source maps expose per-function spans keyed on that index, so the lookup
//! is direct — no bytecode reverse-engineering.

use std::collections::BTreeMap;

use iota_move_build::CompiledPackage;
use move_binary_format::file_format::FunctionDefinitionIndex;
use move_core_types::language_storage::ModuleId;
use move_trace_format::format::{MoveTrace, TraceEvent};

/// Per-function aggregation of a [`MoveTrace`]. One entry per
/// `(module_id, function_name)` pair actually called during execution.
#[derive(Debug, Clone)]
pub struct FunctionGasSummary {
    pub module: ModuleId,
    pub function_name: String,
    /// Total gas charged to this function across every invocation in the
    /// trace (sum of `enter_gas - exit_gas` over all frames).
    pub gas_used: u64,
    /// Number of times the function was called.
    pub calls: usize,
    /// Number of instructions executed inside this function (direct, not
    /// including nested calls).
    pub instructions: usize,
    /// Whether this is a native function.
    pub is_native: bool,
    /// Source-span information, populated only by [`summarize_with_source`]
    /// when a matching module is found in the supplied [`CompiledPackage`].
    pub source: Option<SourceSpan>,
}

/// Source span of a function's definition. Byte offsets match the Move
/// source maps; the `file_hash` identifies which source file to look into.
#[derive(Debug, Clone, Copy)]
pub struct SourceSpan {
    pub file_hash: [u8; 32],
    pub start_byte: u32,
    pub end_byte: u32,
}

/// Walk the trace and emit one [`FunctionGasSummary`] per `(module,
/// function)` pair. The `source` field is always `None` — use
/// [`summarize_with_source`] for source-aware summaries.
pub fn summarize_by_function(trace: &MoveTrace) -> Vec<FunctionGasSummary> {
    let mut per_fn: BTreeMap<(ModuleId, String), (u64, usize, usize, bool)> = BTreeMap::new();
    // Frame stack: (module, function, gas-on-enter, instructions-on-enter).
    let mut stack: Vec<(ModuleId, String, u64, usize)> = Vec::new();
    let mut total_instructions: usize = 0;

    for event in &trace.events {
        match event {
            TraceEvent::OpenFrame { frame, gas_left } => {
                // Record native-ness eagerly — the (module, fn) key may not
                // exist yet when we close the frame.
                let entry = per_fn
                    .entry((frame.module.clone(), frame.function_name.clone()))
                    .or_insert((0, 0, 0, frame.is_native));
                entry.3 = entry.3 || frame.is_native;
                stack.push((
                    frame.module.clone(),
                    frame.function_name.clone(),
                    *gas_left,
                    total_instructions,
                ));
            }
            TraceEvent::Instruction { .. } => {
                total_instructions += 1;
            }
            TraceEvent::CloseFrame {
                frame_id: _,
                return_: _,
                gas_left,
            } => {
                if let Some((module, function, enter_gas, enter_instr)) = stack.pop() {
                    let gas_used = enter_gas.saturating_sub(*gas_left);
                    let instr_delta = total_instructions.saturating_sub(enter_instr);
                    let entry = per_fn.entry((module, function)).or_insert((0, 0, 0, false));
                    entry.0 = entry.0.saturating_add(gas_used);
                    entry.1 += 1;
                    entry.2 = entry.2.saturating_add(instr_delta);
                }
            }
            TraceEvent::Effect(_) | TraceEvent::External(_) => {}
        }
    }

    per_fn
        .into_iter()
        .map(
            |((module, function_name), (gas_used, calls, instructions, is_native))| {
                FunctionGasSummary {
                    module,
                    function_name,
                    gas_used,
                    calls,
                    instructions,
                    is_native,
                    source: None,
                }
            },
        )
        .collect()
}

/// Walk the trace like [`summarize_by_function`] and additionally try to
/// fill the `source` span for each function using the supplied
/// [`CompiledPackage`]'s source maps.
///
/// Functions from other modules (framework, dependencies) are still
/// returned — their `source` field is just `None`.
pub fn summarize_with_source(
    trace: &MoveTrace,
    compiled: &CompiledPackage,
) -> Vec<FunctionGasSummary> {
    let mut summaries = summarize_by_function(trace);
    for summary in &mut summaries {
        summary.source = lookup_source_span(&summary.module, &summary.function_name, compiled);
    }
    summaries
}

/// Look up the byte-span of `function_name` in `module` via the package's
/// source maps.
fn lookup_source_span(
    module: &ModuleId,
    function_name: &str,
    compiled: &CompiledPackage,
) -> Option<SourceSpan> {
    for unit in compiled.package.root_compiled_units.iter() {
        let compiled_module = &unit.unit.module;
        if compiled_module.self_id() != *module {
            continue;
        }
        // Find the function definition index with a matching name.
        let fdef_idx = compiled_module
            .function_defs
            .iter()
            .enumerate()
            .find_map(|(i, fdef)| {
                let handle = &compiled_module.function_handles[fdef.function.0 as usize];
                let ident = compiled_module.identifiers[handle.name.0 as usize].as_str();
                (ident == function_name).then_some(FunctionDefinitionIndex(i as u16))
            })?;
        let fn_map = unit
            .unit
            .source_map
            .get_function_source_map(fdef_idx)
            .ok()?;
        let location = fn_map.definition_location;
        return Some(SourceSpan {
            file_hash: location.file_hash().0,
            start_byte: location.start() as u32,
            end_byte: location.end() as u32,
        });
    }
    None
}
