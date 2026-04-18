# Instruction-level tracing

Capture a structured `MoveTrace` of every frame opened, instruction executed, and value read/written while your transaction runs. Used for post-hoc analysis, programmatic inspection, or as the input to the [IDE integration hook](internal/ide-integration.md).

Prerequisite: code you want to trace in a local Move package. See [local-packages.md](local-packages.md).

Known limitation: the Move VM's `tracer2` panics when the entry point of a PTB command is a native function (e.g. a PTB whose only command is `0x2::hash::blake2b256`). The tracer's `open_initial_frame_` asserts `local_types.len() == function.local_count()`, but for natives `FunctionTypeInfo` only populates parameter types (no `fdef.code.locals`), so the two sides drift and the assertion fires. Calling through a plain Move function (the usual case — entry fns, PTB commands into your own package) pushes a real Move frame first and works fine; natives invoked from inside such a frame are traced normally.

## Configure

Set `DebugConfig::trace = true`. A fresh `MoveTraceBuilder` is created per simulation; the resulting `MoveTrace` lands in `DebugArtifacts::trace`.

```rust
use iota_local_executor::{
    DebugConfig, InMemoryStore, LocalPackage, OfflineExecutor, VmChecks,
};

// (... seed store + install local package ...)

let executor = OfflineExecutor::with_debug(
    ProtocolVersion::MAX,
    1000,
    0,
    0,
    store,
    DebugConfig {
        trace: true,
        ..DebugConfig::default()
    },
)?;
```

## Run and read

```rust
let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
let trace = out.artifacts.trace.expect("DebugConfig::trace was true");

println!("{} events", trace.events.len());
for event in &trace.events {
    match event {
        move_trace_format::format::TraceEvent::OpenFrame { frame, gas_left } => {
            println!("→ {}::{}::{} gas={}", frame.module.address(), frame.module.name(), frame.function_name, gas_left);
        }
        move_trace_format::format::TraceEvent::Instruction { pc, instruction, gas_left, .. } => {
            println!("  pc={pc} {instruction} gas={gas_left}");
        }
        move_trace_format::format::TraceEvent::CloseFrame { gas_left, .. } => {
            println!("← gas={gas_left}");
        }
        _ => {}
    }
}
```

## Typical sizes

Trace volume scales with instruction count, not transaction size. A 10-iteration loop produces ~400 events; real-world staking txs can produce thousands. For large traces, consider post-processing with [ide::summarize_by_function](internal/ide-integration.md) rather than walking raw events.

## Composing with other artifacts

`trace` is orthogonal to `profile` and `capture_debug_prints`. Turn all three on at once to get a full picture; they cover disjoint dimensions:

```rust
DebugConfig {
    capture_debug_prints: true,
    profile: Some(ProfileSink::Capture),
    trace: true,
    ..DebugConfig::default()
}
```

The [effects-parity test](../tests/local_package.rs) `full_debug_config_preserves_effects_on_fixture` verifies this combination doesn't perturb execution — byte-identical `TransactionEffects` with and without all three on.

## Examples and tests

- [../tests/local_package.rs::trace_captures_events_for_fixture_call](../tests/local_package.rs) — asserts `trace.events` is non-empty after a fixture call
- [../tests/ide.rs::summarize_by_function_sees_fixture_calls](../tests/ide.rs) — walks the trace to produce per-function gas summaries
