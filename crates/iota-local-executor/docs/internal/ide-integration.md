# IDE integration hook

Aggregate a `MoveTrace` into per-function gas summaries correlated with source-map byte spans — the raw material a Move IDE plugin needs to draw inline gas overlays, call-jump gutter decorations, or a "hottest function" list.

Prerequisites:

- Execution tracing enabled — see [tracing.md](../tracing.md).
- (Optional) A `CompiledPackage` for your own code if you want source-span resolution.

## Aggregate the trace

`summarize_by_function` walks a `MoveTrace` and emits one `FunctionGasSummary` per `(module_id, function_name)` pair actually executed:

```rust
use iota_local_executor::{summarize_by_function, DebugConfig, VmChecks};

let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
let trace = out.artifacts.trace.expect("trace was enabled");

for summary in summarize_by_function(&trace) {
    println!(
        "{}::{}  gas={} calls={} instructions={} native={}",
        summary.module.name(),
        summary.function_name,
        summary.gas_used,
        summary.calls,
        summary.instructions,
        summary.is_native,
    );
}
```

`gas_used` is the sum of `enter_gas - exit_gas` across every call of that function. `instructions` counts only direct instructions (not nested calls), so the sum of `instructions` over all frames equals the total instruction count.

## Add source spans

If you have the `CompiledPackage` that produced the bytecode (see [local-packages.md](../local-packages.md)), pass it to `summarize_with_source` for file-hash + byte-span lookup:

```rust
use iota_local_executor::summarize_with_source;

let pkg = LocalPackage::compile(/* … */)?;
let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
let trace = out.artifacts.trace.unwrap();

for summary in summarize_with_source(&trace, &pkg.compiled) {
    if let Some(span) = summary.source {
        println!(
            "{}::{}: bytes {}..{} in file {:?}",
            summary.module.name(),
            summary.function_name,
            span.start_byte,
            span.end_byte,
            hex::encode(span.file_hash),
        );
    }
}
```

Functions from other modules (framework, dependencies) still appear in the output — their `source` is just `None`.

## Wiring up an IDE

A minimal IDE integration:

1. Take a transaction from the user (debug-run, test harness, etc.).
2. Simulate with `DebugConfig::trace = true`.
3. Call `summarize_with_source(&trace, &pkg.compiled)`.
4. Resolve `span.file_hash` to a source file (the compiled package's `source_maps` expose the mapping from hash → path).
5. For each `(file, span, gas_used)` render a gutter annotation or inline overlay at `span.start_byte`.

The `SourceSpan` fields are byte-offsets into the source file, matching the format Move's source maps use everywhere else — no extra conversion is needed to integrate with tree-sitter, an LSP server, or an `iota move test` harness.

## Example output

```
counter::create       gas=842   calls=1  instructions=18  native=false
counter::increment    gas=611   calls=1  instructions=12  native=false
object::new           gas=100   calls=1  instructions=0   native=true
dynamic_field::add    gas=223   calls=1  instructions=0   native=true
```

## Examples and tests

- [../../tests/ide.rs::summarize_by_function_sees_fixture_calls](../../tests/ide.rs) — full round-trip including source-span resolution
- [../../src/ide.rs](../../src/ide.rs) — implementation
