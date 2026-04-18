# Remote-fetch caching and `ChainInfo` reuse

Skip redundant network round-trips when running many simulations against a live node. Two complementary mechanisms:

1. **Persistent object cache** — each networked executor owns a store that keeps fetched objects across simulations.
2. **`ChainInfo`** — fetch the protocol version + epoch + gas price **once**, then hand it to many executors.

## Object cache (automatic)

Every `JsonRpcExecutor` / `GrpcExecutor` / `GraphqlExecutor` owns a `shared_store: *Store`. Every `simulate_*` call uses that same store, so framework packages and any immutable object are fetched at most once per executor.

```rust
let executor = JsonRpcExecutor::new(client).await?;
executor.simulate_transaction(tx1, VmChecks::Disabled).await?;   // fetches pkg 0x2
executor.simulate_transaction(tx2, VmChecks::Disabled).await?;   // cache hit on 0x2
executor.simulate_transaction(tx3, VmChecks::Disabled).await?;   // cache hit on 0x2
```

### Controlling the cache

```rust
executor.store()              // &JsonRpcStore — peek / install a LocalPackage override
executor.new_store()          // fresh, isolated store (pair with *_with_debug_using)
executor.clear_cache()        // drop everything, next simulate re-fetches
```

Use `new_store()` when a particular run must not be contaminated by prior cache state (e.g. a benchmark measurement).

## Sharing chain state across executors

The four values every networked executor fetches at construction (protocol version, reference gas price, epoch id, epoch timestamp) rarely change over short windows. `ChainInfo` captures them:

```rust
use iota_local_executor::{ChainInfo, JsonRpcExecutor};

let info = ChainInfo::fetch_from_json_rpc(&client).await?;   // one round-trip

for tx in txs {
    let executor = JsonRpcExecutor::with_chain_info(client.clone(), info)?;
    //                                                                ^^^ zero extra fetches
    let r = executor.simulate_transaction(tx, VmChecks::Disabled).await?;
    // …
}
```

Equivalents for the other backends: `ChainInfo::fetch_from_grpc`, `ChainInfo::fetch_from_graphql`, and `GrpcExecutor::with_chain_info` / `GraphqlExecutor::with_chain_info`.

### With a debug config

```rust
let info = ChainInfo::fetch_from_json_rpc(&client).await?;
let executor = JsonRpcExecutor::with_chain_info_and_debug(
    client,
    info,
    DebugConfig { trace: true, ..DebugConfig::default() },
)?;
```

## When to refresh

`ChainInfo` is a snapshot. The values that can change:

- `epoch_id` and `epoch_timestamp_ms` — when a new epoch begins (typically once per day).
- `reference_gas_price` — rare, but can be set by governance.
- `protocol_version` — rare, tied to network upgrades.

For dev-inspect use where you just want effects, any recent snapshot is fine. For dry-run of a time-sensitive transaction (e.g. one that reads the current epoch), refresh between epochs or just let the executor fetch fresh via `JsonRpcExecutor::new`.

## Examples and tests

- [../../src/chain_info.rs](../../src/chain_info.rs) — the full `ChainInfo::fetch_from_*` implementations
- [../../src/json_rpc_executor.rs](../../src/json_rpc_executor.rs) — `store`, `new_store`, `clear_cache`, `with_chain_info*` APIs
