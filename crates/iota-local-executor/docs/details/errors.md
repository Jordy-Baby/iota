# Structured errors

`LocalExecError` tags some simulation failures with the layer they came from, so retry logic and user-facing error messages can be wired up sensibly.

## Coverage

Currently only one layer produces a tagged error:

- **`LocalExecError::Validation`** — pre-execution validation failed: signature wrong, tx malformed, gas budget below minimum, denied object. Not retryable — the caller has to fix the tx.

Failures from the remote fetch layer (JSON-RPC / gRPC / GraphQL) and from inside the Move VM during simulation still propagate up as raw `anyhow::Error`. They may grow their own `LocalExecError` variants in the future; for now `downcast_ref::<LocalExecError>()` returns `None` for those.

## Downcasting

Every simulate method returns `anyhow::Result`. `LocalExecError` is tagged in via `map_err` so `downcast_ref` reveals it:

```rust
use iota_local_executor::{JsonRpcExecutor, LocalExecError, VmChecks};

match executor.simulate_transaction(tx, VmChecks::Enabled).await {
    Ok(r) => { /* inspect r.effects.status() */ }
    Err(e) => {
        if let Some(tagged) = e.downcast_ref::<LocalExecError>() {
            if tagged.is_validation() {
                // present to the user
            }
        } else {
            // plain anyhow::Error — treat as generic failure (network, VM, etc.)
        }
    }
}
```

The `Display` impl prefixes the message with `[validation]` so logs are immediately grep-able.

## Constructing a `LocalExecError` in your own code

If you're wrapping the executor, mint your own tagged errors:

```rust
use iota_local_executor::LocalExecError;

let err = LocalExecError::validation(
    "missing input object",
    anyhow::anyhow!("object 0x… not in store"),
);
```

`validation(...)` takes `(context, source)` where `source` is any `impl Into<anyhow::Error>`.

## Examples and tests

- [../../tests/error_tagging.rs](../../tests/error_tagging.rs) — validation-tagged error when a transaction has a bogus gas ref; `Display` prefix checks
- [../../src/error.rs](../../src/error.rs) — the type definitions
