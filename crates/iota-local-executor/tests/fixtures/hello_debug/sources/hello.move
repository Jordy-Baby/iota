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

    /// Print a compile-time-known string via `debug::print`. The exact text
    /// is asserted by `tests/debug_config.rs`.
    public fun greet() {
        let msg = b"hello from iota-local-executor";
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
