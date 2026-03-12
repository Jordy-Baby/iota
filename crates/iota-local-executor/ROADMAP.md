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
