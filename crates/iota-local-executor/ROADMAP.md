# iota-local-executor — Roadmap

## Goal

Provide a crate that allows running Move VM transactions **locally** (on the client side) without sending them to a node. The user provides transaction bytes (same format as the node JSON-RPC API), and this component:

1. Parses the transaction data
2. Fetches the required objects from a remote node (via JSON-RPC, gRPC, or GraphQL)
3. Executes the transaction locally using the same Move VM and execution engine as a full node
4. Returns the execution result (effects, events, return values) to the caller

This enables fast, private dry-run and dev-inspect workflows without relying on a node's compute resources.

## Architecture

Four executor types are provided:

- **`JsonRpcExecutor`** — fetches objects via JSON-RPC
- **`GrpcExecutor`** — fetches objects via gRPC
- **`GraphqlExecutor`** — fetches objects via GraphQL
- **`OfflineExecutor`** — fully offline, no network access

Each network-backed executor pre-fetches input objects in batch, then uses a local cache store (`JsonRpcStore`, `GrpcStore`, `GraphqlStore`) that also handles on-demand fetching of dynamically loaded objects during execution.

## Status

### Done

- [x] Basic crate structure with `JsonRpcExecutor` and `JsonRpcStore`
- [x] `JsonRpcStore` implements `BackingStore` (ObjectStore + BackingPackageStore + ChildObjectResolver)
- [x] Pre-fetches input objects via batch `multi_get_object_with_options` RPC call
- [x] On-demand fetching of dynamically loaded objects during execution (packages, child objects)
- [x] `simulate_transaction()` — unified dry-run / dev-inspect via `VmChecks` parameter
- [x] Mock gas object creation when no gas is provided
- [x] Automatic object version reconciliation (updates stale tx refs to match fetched versions)
- [x] Configurable object fetch mode: exact transaction versions (default) or latest versions
- [x] `OfflineExecutor` with `InMemoryStore` for fully offline execution (no network access)
- [x] Example: pure function call (blake2b256) — no objects needed
- [x] Example: staking transaction — shared + owned objects fetched from node
- [x] Example: fully offline execution with hardcoded framework packages
- [x] `simulate_signed_transaction()` — optional signature verification before execution

### TODO

- [x] ~~**Object version handling**~~ Done.
- [ ] **Caching and reuse**: Allow the `JsonRpcStore` to persist across multiple executions, so packages and immutable objects don't need to be re-fetched.
- [x] ~~**GraphQL and gRPC support**~~ Done.
- [ ] **Error reporting**: Improve error messages to clearly distinguish between local execution errors and remote fetch errors.
- [ ] **Result formatting**: Add helpers to format `SimulateTransactionResult` into user-friendly output (similar to `DevInspectResults` / `DryRunTransactionBlockResponse` from JSON-RPC types).
- [ ] **Protocol config caching**: Cache the protocol config and epoch info so that repeated executions don't re-fetch them.
- [ ] **Gas estimation**: Return gas cost estimates from the local execution.
- [ ] **Event decoding**: Add helpers to decode Move events from the execution result.
- [ ] **Multi-transaction support**: Support executing a sequence of transactions where later transactions see the state changes from earlier ones (useful for testing flows).
- [x] ~~**Offline mode**~~ Done.
- [x] ~~**Integration tests**~~ Done.
- [ ] **Benchmarks**: Compare local execution performance vs. remote dry-run to quantify the benefit.

## Debug & Tracing Features

A key advantage of the local executor over remote dry-run/dev-inspect is the ability to capture debug output and execution traces that nodes never expose. Below are the options, what changes they require, and how the output could be used.

### 1. Move `debug::print` Output

**Current state:** The Move VM executor is created with `silent=true` (`execution.rs:66`), which replaces `debug::print` / `debug::print_stack_trace` native functions with no-ops. Additionally, the actual print logic is behind a `testing` feature gate in `move-stdlib-natives`.

**What needs to change:**
- Make the `silent` flag configurable on `ExecutionEnv` (e.g. a new field or a parameter to `ExecutionEnv::new`).
- Enable the `testing` feature on the `move-stdlib-natives` dependency (or the transitive path through `iota-execution`).
- Expose a user-facing option on each executor (e.g. `with_debug_output(true)`) that sets `silent=false`.

**Output behavior:** Currently `debug::print` writes to **stdout** via `println!("[debug] {value}")`. This means:
- Output is immediately visible in the terminal when running CLI tools or examples.
- For programmatic use, stdout could be captured (e.g. `gag` crate or OS-level redirect).

**Advanced option — structured capture:** Modify the Move `native_print` implementation to collect output into a thread-local or VM-scoped buffer instead of using `println!`. Return the collected debug log as part of `SimulateTransactionResult`. This requires changes in `external-crates/move/crates/move-stdlib-natives/src/debug.rs` and threading the buffer through the execution context.

**Benefit:** Developers can add `debug::print` statements in their Move code and see the output during local simulation — something impossible with remote dry-run. This bridges the gap between `iota move test` (where debug prints work) and real transaction simulation.

### 2. Execution Tracing / Gas Profiling

**Current state:** The `iota_execution::executor()` function accepts an `enable_profiler: Option<PathBuf>` parameter (currently passed as `None`). When set, the Move VM writes a gas profiling trace to the given path.

**What needs to change:**
- Expose the `enable_profiler` option through `ExecutionEnv` and each executor's builder API.
- Optionally return the trace data as part of the result instead of writing to a file.

**Output:** A JSON gas profile that can be visualized with tools like `move-gas-profiler`, showing per-instruction gas costs, function call stacks, and hotspots.

**Benefit:** Enables gas optimization workflows entirely locally — developers can profile their Move transactions without deploying to a network or relying on node-side tooling.

### 3. Event-Level Tracing

**Current state:** Transaction events are already captured in `SimulateTransactionResult::events`, but they are returned as raw BCS-encoded `TransactionEvents`.

**What needs to change:**
- Add event decoding helpers (already tracked in the TODO list above).
- Optionally enable verbose Move VM tracing (instruction-level logs) via the executor's tracing configuration.

**Benefit:** Combined with debug prints and gas profiling, this gives a complete picture of what happened during execution — useful for debugging failed transactions, understanding gas consumption, and validating Move logic before on-chain submission.
