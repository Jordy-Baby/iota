# Move `debug::print` output

Capture `std::debug::print(&value)` output from your Move code during local simulation, either to stdout or into a structured `Vec<String>` you can assert on.

Prerequisite: code containing `debug::print` calls in a local Move package. See [local-packages.md](local-packages.md).

## Put `debug::print` in your Move code

```move
module hello::hello {
    use std::debug;

    public fun greet() {
        let msg = b"hello from iota-local-executor";
        debug::print(&msg);
    }
}
```

`debug::print` can print any value with a layout — primitives, structs, vectors. Internally it's formatted by the Move VM's type-layout resolver, same as `iota move test` does.

## Option A — stdout capture

Just set `capture_debug_prints = true`. Prints land on stdout with a `[debug]` prefix:

```rust
use iota_local_executor::{DebugConfig, OfflineExecutor, VmChecks};

let executor = OfflineExecutor::with_debug(
    ProtocolVersion::MAX,
    1000,
    0,
    0,
    store,
    DebugConfig {
        capture_debug_prints: true,
        ..DebugConfig::default()
    },
)?;

executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
// Your terminal now has: `[debug] 0x68656c6c6f2066726f6d20...`
```

Prefix the `cargo test` invocation with `--nocapture` to see the output, or add your own `gag::BufferRedirect` to capture stdout in-process.

## Option B — structured capture (recommended)

For programmatic access, set `structured_debug_capture = true` in addition. Prints are routed through a thread-local sink into `DebugArtifacts::debug_prints` — no stdout redirection needed.

```rust
let executor = OfflineExecutor::with_debug(
    /* … */,
    DebugConfig {
        capture_debug_prints: true,
        structured_debug_capture: true,
        ..DebugConfig::default()
    },
)?;

let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
for line in out.artifacts.debug_prints {
    println!("captured: {line}");
    // captured: [debug] 0x68656c6c6f2066726f6d20...
}
```

The sink is installed on the calling thread at simulate time and drained afterwards. Safe to call concurrently across threads — each thread sees its own sink.

## How values are formatted

`debug::print` uses the Move VM's annotated-value printer. Byte-string literals like `b"hello"` print as the hex of their raw bytes (`0x68656c6c6f`). Structs print with field names. Vectors print with index-based rendering. This matches `iota move test` output.

## Examples and tests

- [../tests/local_package.rs::greet_debug_prints_captured_into_artifacts](../tests/local_package.rs) — uses `structured_debug_capture`, asserts the `[debug]` line contains the expected hex
- [../tests/fixtures/hello_debug/sources/hello.move](../tests/fixtures/hello_debug/sources/hello.move) — the fixture Move module with a `debug::print` call
