# iota-local-executor — Roadmap

## Goal

Provide a crate that allows running Move VM transactions **locally** (on the client side) without sending them to a node. The user provides transaction bytes (same format as the node JSON-RPC API), and this component:

1. Parses the transaction data
2. Fetches the required objects from a remote node (via JSON-RPC)
3. Executes the transaction locally using the same Move VM and execution engine as a full node
4. Returns the execution result (effects, events, return values) to the caller

This enables fast, private dry-run and dev-inspect workflows without relying on a node's compute resources.

## Architecture

Two executor types are provided:

### LocalExecutor (fetches objects from a node)
```
┌────────────────┐      JSON-RPC         ┌──────────────┐
│ LocalExecutor  │ ──── fetch objects ──▶│  IOTA Node   │
│                │ ◀── objects ──────────│              │
│  ┌───────────┐ │                       └──────────────┘
│  │RemoteStore│ │  (caches objects in memory)
│  └───────────┘ │
│  ┌───────────┐ │
│  │ Move VM   │ │  (iota-execution engine)
│  └───────────┘ │
│                │ ──▶ SimulateTransactionResult
└────────────────┘
```

### OfflineExecutor (fully offline, no network)
```
┌──────────────────┐
│ OfflineExecutor  │
│  ┌─────────────┐ │  (all objects provided upfront)
│  │InMemoryStore│ │
│  └─────────────┘ │
│  ┌─────────────┐ │
│  │  Move VM    │ │  (iota-execution engine)
│  └─────────────┘ │
│                  │ ──▶ SimulateTransactionResult
└──────────────────┘
```

## Status

### Done

- [x] Basic crate structure with `LocalExecutor` and `RemoteStore`
- [x] `RemoteStore` implements `BackingStore` (ObjectStore + BackingPackageStore + ChildObjectResolver)
- [x] Pre-fetches input objects via batch `multi_get_object_with_options` RPC call
- [x] On-demand fetching of dynamically loaded objects during execution (packages, child objects)
- [x] `dev_inspect()` method — relaxed checks, suitable for exploration
- [x] `dry_run()` method — strict checks, equivalent to node dry-run
- [x] Mock gas object creation when no gas is provided
- [x] Automatic object version reconciliation (updates stale tx refs to match fetched versions)
- [x] `OfflineExecutor` with `InMemoryStore` for fully offline execution (no network access)
- [x] Example: pure function call (blake2b256) — no objects needed
- [x] Example: staking transaction — shared + owned objects fetched from node
- [x] Example: fully offline execution with hardcoded framework packages

### TODO

- [ ] **Object version handling**: The current RPC only fetches the latest version of an object. For strict dry-run with owned objects at specific versions, we may need `tryGetPastObject` or version-aware fetching.
- [ ] **Shared object version resolution**: Shared objects require consensus-assigned versions. The local executor currently uses the latest version, which may differ from what the network would assign. Consider fetching the scheduled version or documenting this limitation.
- [ ] **Caching and reuse**: Allow the `RemoteStore` to persist across multiple executions, so packages and immutable objects don't need to be re-fetched.
- [ ] **GraphQL support**: Add an alternative `RemoteStore` backend that uses the GraphQL API instead of JSON-RPC, which may be more efficient for complex queries.
- [ ] **Error reporting**: Improve error messages to clearly distinguish between local execution errors and remote fetch errors.
- [ ] **Result formatting**: Add helpers to format `SimulateTransactionResult` into user-friendly output (similar to `DevInspectResults` / `DryRunTransactionBlockResponse` from JSON-RPC types).
- [ ] **Protocol config caching**: Cache the protocol config and epoch info so that repeated executions don't re-fetch them.
- [ ] **Gas estimation**: Return gas cost estimates from the local execution.
- [ ] **Event decoding**: Add helpers to decode Move events from the execution result.
- [ ] **Multi-transaction support**: Support executing a sequence of transactions where later transactions see the state changes from earlier ones (useful for testing flows).
- [x] ~~**Offline mode**: Support a fully offline mode where all objects are provided upfront (no network access needed).~~ Done via `OfflineExecutor`.
- [ ] **Integration tests**: Add integration tests that run against a local test cluster.
- [ ] **Benchmarks**: Compare local execution performance vs. remote dry-run to quantify the benefit.
