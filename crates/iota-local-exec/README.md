# iota-local-exec

A thin command-line wrapper around [`OfflineExecutor`](../iota-local-executor/src/offline_executor.rs) for running a **single** `MoveCall` programmable transaction against locally-compiled Move packages — with gas profiling, instruction tracing, `debug::print` capture, and CI-friendly assertions.

See also the underlying library crate: [`iota-local-executor`](../iota-local-executor/README.md).

```sh
cargo run -p iota-local-exec -- <flags>
# or, after a release build:
./target/release/iota-local-exec <flags>
```

## Flags

### Packages

| Flag                  | Description                                                                                                                                  |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `--package PATH[=ID]` | Compile the Move package at `PATH` and install it under `ID`. If `ID` is omitted, IDs are auto-assigned starting at `0x42`. Repeatable.      |
| `--named-address K=V` | Override the package's named-address mapping for the synthetic ID. Repeatable. Defaults to the first `[addresses]` entry whose value is `_`. |

### Transaction body

| Flag                     | Description                                                                                           |
| ------------------------ | ----------------------------------------------------------------------------------------------------- |
| `--call PKG::MODULE::FN` | Move function to invoke. `PKG` may be elided if exactly one package is loaded (e.g. `hello::sum_to`). |
| `--arg VALUE`            | Typed argument. See [Argument grammar](#argument-grammar). Repeatable.                                |
| `--type-arg TYPE`        | Type argument, e.g. `0x2::iota::IOTA`. Repeatable.                                                    |

### Sender & gas

| Flag              | Default        | Description                                                    |
| ----------------- | -------------- | -------------------------------------------------------------- |
| `--sender 0xADDR` | `0x0`          | Sender address.                                                |
| `--gas-budget N`  | `100_000_000`  | Gas budget in nanos.                                           |
| `--gas-price N`   | `1000`         | Gas price.                                                     |
| `--gas-coin 0xID` | auto-mint mock | Gas payment object. If omitted, the executor mocks a gas coin. |

### Authenticator (optional)

| Flag                        | Description                                                                                                        |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `--authenticator PKG::M::F` | Wrap the transaction in a `MoveAuthenticator` pointing at this function.                                           |
| `--auth-arg VALUE`          | Authenticator-function argument. Same grammar as `--arg`. Repeatable.                                              |
| `--account-object 0xID`     | Abstract-account object that `MoveAuthenticator` authenticates against. **Required** with `--authenticator` in v1. |

### Simulation

| Flag                           | Default       | Description                                                               |
| ------------------------------ | ------------- | ------------------------------------------------------------------------- |
| `--mode {dev-inspect,dry-run}` | `dev-inspect` | `dev-inspect` relaxes Move VM checks; `dry-run` performs full validation. |
| `--no-profile`                 | off           | Disable gas profiling.                                                    |
| `--no-trace`                   | off           | Disable instruction-level tracing.                                        |
| `--no-debug-prints`            | off           | Disable structured `debug::print` capture.                                |

### Remote state

| Flag                         | Default | Description                                                                                                                                                                  |
| ---------------------------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--remote-grpc URL`          | —       | Connect to a gRPC endpoint and snapshot live chain state (protocol version, reference gas price, current epoch) so the executor mirrors the chain instead of using defaults. |
| `--remote-object 0xID[@VER]` | —       | Prefetch a remote object via `--remote-grpc`. Omit `@VER` for the latest version. Repeatable.                                                                                |

Without `--remote-grpc`, all referenced objects must come from `--package` (compiled in-process) or `--gas-coin` (auto-minted). With `--remote-grpc`, anything not loaded locally is fetched once and cached in the in-memory store before execution.

### Assertions (CI)

| Flag                                       | Description                                                                                                  |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `--require-success`                        | Exit non-zero if the transaction status is not `success`.                                                    |
| `--assert-gas-below N`                     | Exit non-zero unless net gas usage (computation + storage − rebate) is strictly less than `N`.               |
| `--assert-function-gas-below module::fn=N` | Exit non-zero unless the aggregated gas spent in `module::fn` (from the trace) is less than `N`. Repeatable. |

### Output

| Flag                          | Default | Description                                                               |
| ----------------------------- | ------- | ------------------------------------------------------------------------- |
| `--out-dir PATH`              | —       | Write per-file artifacts into `PATH`. Directory is created if missing.    |
| `--stdout {json,pretty,none}` | `json`  | Format the top-level summary printed to stdout (or suppress it entirely). |

## Argument grammar

Each `--arg` / `--auth-arg` value is parsed with a small fixed grammar. Unrecognised forms are rejected with a clear error.

| Form                         | Meaning                                                  |
| ---------------------------- | -------------------------------------------------------- |
| `10u8`, `10u16`, …, `10u256` | Typed integer → pure argument                            |
| `true`, `false`              | Bool → pure argument                                     |
| `@0xADDR`                    | Address → pure argument                                  |
| `"text"`                     | UTF-8 string → pure `vector<u8>`                         |
| `object(0xID)`               | Resolve `0xID` in the store, pass as an object reference |

Unsuffixed integers (e.g. `10`) are rejected — always use a type suffix to remove ambiguity. Vector literals (`[1,2,3]u64`) are **not** supported in v1; use Rust tests for complex inputs.

## Output files

When `--out-dir PATH` is set, the following files are written (each only if the corresponding artifact is produced):

| File                | Contents                                                           |
| ------------------- | ------------------------------------------------------------------ |
| `effects.json`      | Transaction effects.                                               |
| `events.json`       | Emitted events (raw BCS payload per event).                        |
| `debug-prints.txt`  | Captured `debug::print` lines, one per line.                       |
| `gas-profile.json`  | Speedscope gas profile.                                            |
| `trace.json`        | Raw `MoveTrace` JSON.                                              |
| `function-gas.json` | Per-function gas aggregation (matches `summarize_by_function`).    |
| `summary.json`      | Top-level summary — also printed to stdout unless `--stdout none`. |

## Summary schema

```json
{
  "status": "success",
  "gas": {
    "computation": 1000,
    "storage": 2345,
    "rebate": 1976,
    "net": 1369
  },
  "debug_prints": 2,
  "events": 1,
  "function_gas_top": [
    { "module": "0x42::hello", "function": "sum_to", "gas_used": 842, "calls": 1, "is_native": false }
  ],
  "assertions": [
    { "name": "auth::authenticate < 10000", "passed": true, "observed": 411 }
  ],
  "artifacts": {
    "effects": "effects.json",
    "gas_profile": "gas-profile.json",
    "trace": "trace.json",
    "function_gas": "function-gas.json",
    "summary": "summary.json"
  }
}
```

## Exit codes

| Code | Meaning                                                               |
| ---- | --------------------------------------------------------------------- |
| `0`  | Ran to completion, all assertions passed.                             |
| `1`  | Ran, but an assertion failed (or `--require-success` with failed tx). |
| `2`  | Compilation error.                                                    |
| `3`  | Executor or fetch error (including the unimplemented remote flags).   |

## Examples

Single call against a local package:

```sh
iota-local-exec \
  --package ./my-package \
  --call my_pkg::math::factorial --arg 10u64 \
  --out-dir ./artifacts
```

With a gas-cap assertion for CI:

```sh
iota-local-exec \
  --package ./my-package \
  --call my_pkg::math::factorial --arg 10u64 \
  --require-success \
  --assert-gas-below 50_000_000 \
  --assert-function-gas-below "math::factorial=5000000" \
  --stdout none
```

Multiple packages:

```sh
iota-local-exec \
  --package ./auth=0x42 \
  --package ./app=0x43 \
  --call 0x43::app::run --arg "ok" \
  --out-dir ./run
```

## Limitation — multi-command PTBs not supported

The v1 CLI accepts only a single `--call` (one `MoveCall` PTB). Full `iota client ptb` script syntax (split-coin, merge, dotted-fields, named results) is **not duplicated** here to avoid coupling `iota-local-executor` to the `iota` binary crate's dep tree (`aws-sdk-kms`, `fastcrypto`, `reqwest`, `miette`, `clap_complete`, …).

A future refactor would extract `iota::client_ptb::{lexer, token, parser, ast, error}` into a standalone `iota-ptb-parser` crate and refactor `PTBBuilder` to be generic over a `trait ObjectSource` (implemented by both `ReadApi` and `BackingStore`). Both the `iota` binary and `iota-local-exec` would then share the full PTB grammar. Until that refactor lands, use Rust tests (`tests/*.rs` with `OfflineExecutor` directly) for multi-command scenarios.
