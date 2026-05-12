// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

/// Tiny fixture module used by the `iota-local-executor` debug-tracing tests.
///
/// Everything here is picked to exercise a different knob of the local
/// debugging feature end-to-end:
///
/// - `greet` — uses `std::debug::print` so the debug-print capture test can
///   assert on stdout (Phase 1) or on `DebugArtifacts::debug_prints` (Phase 2).
/// - `work` — runs a trivial loop so the gas profiler records a non-zero
///   amount of work and the Speedscope output has at least one interior
///   frame.
/// - `sum_to` — a simple non-generic function whose frame the Move VM
///   tracer can represent without the dev-inspect synthetic-wrapper quirks
///   that made the blake2b fixture unusable in Checkpoint A.
module hello::hello {
    use std::debug;
    use std::string;

    /// Print a compile-time-known string via `debug::print`. The exact text
    /// is asserted by `tests/debug_config.rs`. The print is delegated to a
    /// nested helper so `debug::print_stack_trace()` has at least one caller
    /// frame to render — see the note on `print_with_trace`.
    public fun greet() {
        print_with_trace(string::utf8(b"hello from iota-local-executor"));
    }

    /// Emits a stack-trace dump followed by the value. Note: the Move VM's
    /// `print_stack_trace` native walks the *call stack* and does not include
    /// the currently-active frame, so `print_with_trace` itself will NOT
    /// appear in the trace output — only its callers (here: `greet`) will.
    /// To see this function in the trace, the print would have to be moved
    /// one frame deeper.
    fun print_with_trace(msg: string::String) {
        debug::print_stack_trace();
        debug::print(&msg);
    }

    /// A tiny arithmetic function the tracer and profiler can record.
    public fun sum_to(n: u64): u64 {
        let mut total: u64 = 0;
        let mut i: u64 = 0;
        while (i < n) {
            total = total + i;
            i = i + 1;
        };
        total
    }

    /// Slightly more work than `sum_to`, for profiler hot-spot tests.
    public fun work(rounds: u64): u64 {
        let mut acc: u64 = 1;
        let mut i: u64 = 0;
        while (i < rounds) {
            acc = acc + i * 3 + 7;
            i = i + 1;
        };
        acc
    }
}
