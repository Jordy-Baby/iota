# iota-local-executor

Run IOTA Move transactions **on the client side**, without sending them to a node.

Given transaction bytes in standard BCS format, this crate fetches the required objects, executes the transaction through the same Move VM a full node uses, and returns the effects, events, and return values — all locally.

## Executors

Pick the backend that matches how you get at objects:

| Executor          | Backend                    | Example                                                                            |
| ----------------- | -------------------------- | ---------------------------------------------------------------------------------- |
| `OfflineExecutor` | In-memory only, no network | [examples/offline_dev_inspect.rs](examples/offline_dev_inspect.rs)                 |
| `GrpcExecutor`    | gRPC                       | [tests/e2e_executor_comparison.rs](tests/e2e_executor_comparison.rs)               |
| `GraphqlExecutor` | GraphQL                    | API mirrors `GrpcExecutor`; see [src/graphql_executor.rs](src/graphql_executor.rs) |

The two networked executors pre-fetch input objects in batch, cache them across calls, and lazily load anything the VM pulls in during execution.

## Related crates

- [iota-local-exec](../iota-local-exec/) — one-shot CLI wrapper.

## Features

The main workflows a developer reaches for when building against the crate. Each link opens a short doc with the full workflow — from nothing to a working output.

- **Dev-inspect and dry-run** — run a transaction locally for fast, private iteration. → [docs/dev-inspect.md](docs/dev-inspect.md)
- **Local Move packages with synthetic IDs** — compile a Move package in-process and wire it to a caller-chosen `ObjectID` (e.g. `0x42`) so transactions can call into it without publishing. → [docs/local-packages.md](docs/local-packages.md)
- **`debug::print` capture** — emit or capture Move `debug::print` output, with structured in-memory capture for programmatic assertions. → [docs/debug-prints.md](docs/debug-prints.md)
- **Gas profiling** — Speedscope-format per-instruction gas profile, viewable in https://www.speedscope.app/, with automatic merging across VM sessions (e.g. authenticator + body). → [docs/gas-profiling.md](docs/gas-profiling.md)
- **Gas estimation** — `GasEstimate::from_effects(...)` + `suggested_budget_with_headroom(1.2)` for picking a submission budget. → [docs/gas-estimation.md](docs/gas-estimation.md)
- **Instruction-level tracing** — full `MoveTrace` of frames, instructions, and value reads/writes for post-hoc analysis. → [docs/tracing.md](docs/tracing.md)
- **Event decoding** — `executor.decode_events(...)` → fully-annotated `MoveValue`s per event, ready to pattern-match. → [docs/events.md](docs/events.md)
- **`MoveAuthenticator` verification** — validate custom Move-code signatures by running the authenticator function in the VM. → [docs/move-authenticator.md](docs/move-authenticator.md)

## Details

Supporting docs referenced from the main features above — look here once you hit a specific need (retry logic, multi-step flows, display helpers, etc.).

- **Chained (multi-transaction) execution** — each tx sees the state changes of the previous; useful for testing flows or reproducing history. → [docs/details/multi-tx.md](docs/details/multi-tx.md)
- **Remote-fetch caching and `ChainInfo` reuse** — persistent object cache per executor, plus one-fetch-many-executors chain state. → [docs/details/remote-caching.md](docs/details/remote-caching.md)
- **Authenticator gas-optimisation** — profile a `MoveAuthenticator`'s function locally and iterate on its Move code. → [docs/details/authenticator-gas-optimization.md](docs/details/authenticator-gas-optimization.md)
- **Structured errors** — `LocalExecError::Validation` so callers can distinguish pre-execution validation failures from generic errors. → [docs/details/errors.md](docs/details/errors.md)

## Internal and integration docs

For people extending the crate or building tools on top of it, rather than simulating transactions with it.

- **IDE integration hook** — aggregate a `MoveTrace` into per-function gas summaries with source-map byte spans, ready for inline overlays. → [docs/internal/ide-integration.md](docs/internal/ide-integration.md)

## Layout

- `src/` — executors, stores, and the debug/tracing/profiling surface.
- `examples/` — runnable end-to-end examples.
- `tests/` — `DebugConfig`, `LocalPackage`, cross-executor equivalence, per-feature tests.
- `docs/` — one doc per user-facing feature, with the full workflow.
- `docs/details/` — side-detail docs referenced from the main features.
- `docs/internal/` — extension-point and maintainer docs (IDE hook).
