# Structured errors

`LocalExecError` tags simulation failures with the layer they came from, so retry logic and user-facing error messages can be wired up sensibly.

## The three categories

- **`LocalExecError::Fetch`** — the remote node (JSON-RPC / gRPC / GraphQL) errored, or a fetched object failed to deserialise. Typically retryable after a backoff.
- **`LocalExecError::Validation`** — pre-execution validation failed: signature wrong, tx malformed, gas budget below minimum, denied object. Not retryable — the caller has to fix the tx.
- **`LocalExecError::Execution`** — the Move VM rejected the tx during simulation (abort, arithmetic error, storage error). The `effects` on a normal result already carry this information when a simulate call **returns** an `Ok` with a failed status; this enum variant is for the cases where the error bubbles out as a `Result::Err` instead.

## Downcasting

Every simulate method returns `anyhow::Result`. `LocalExecError` is tagged in via `map_err` so `downcast_ref` reveals it:

```rust
use iota_local_executor::{JsonRpcExecutor, LocalExecError, VmChecks};

match executor.simulate_transaction(tx, VmChecks::Enabled).await {
    Ok(r) => { /* inspect r.effects.status() */ }
    Err(e) => {
        if let Some(tagged) = e.downcast_ref::<LocalExecError>() {
            if tagged.is_fetch() {
                // network blip — retry with backoff
            } else if tagged.is_validation() {
                // present to the user
            } else if tagged.is_execution() {
                // inspect the source chain for more context
            }
        } else {
            // plain anyhow::Error — treat as generic failure
        }
    }
}
```

The `Display` impl adds a `[fetch] / [validation] / [execution]` prefix so logs are immediately grep-able.

## Constructing a `LocalExecError` in your own code

If you're wrapping the executor, mint your own tagged errors:

```rust
use iota_local_executor::LocalExecError;

let err = LocalExecError::fetch(
    "fetching package 0x2",
    anyhow::anyhow!("connection reset"),
);
```

`fetch(...)` / `validation(...)` / `execution(...)` all take `(context, source)` where `source` is any `impl Into<anyhow::Error>`.

## Examples and tests

- [../../tests/error_tagging.rs](../../tests/error_tagging.rs) — validation-tagged error when a transaction has a bogus gas ref; `Display` prefix checks
- [../../src/error.rs](../../src/error.rs) — the type definitions
