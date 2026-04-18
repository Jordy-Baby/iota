# Dev-inspect and dry-run

Run a Move transaction through the same VM a node uses, locally, to inspect its effects before submitting.

## The two modes

- **`VmChecks::Enabled`** — dry-run mode. Full checks: signatures, gas, type-checking, object versions. Semantically equivalent to submitting a transaction and then rolling back. Use this when you want to know "would this succeed as-is?".
- **`VmChecks::Disabled`** — dev-inspect mode. Relaxed type-checking (return values don't need `drop`), no signature requirement. Use this to read values from Move code or test functions that aren't entry points.

## End-to-end: transaction bytes → effects

Given a base64-encoded transaction (the same format the node's JSON-RPC API accepts), run it locally with the `OfflineExecutor` and inspect the result:

```rust
use iota_framework::BuiltInFramework;
use iota_local_executor::{InMemoryStore, OfflineExecutor, VmChecks};
use iota_protocol_config::ProtocolVersion;
use iota_types::{effects::TransactionEffectsAPI, transaction::TransactionData};

// Seed with the built-in framework packages — zero network access.
let mut store = InMemoryStore::new();
for obj in BuiltInFramework::genesis_objects() {
    store.insert(obj);
}

let executor = OfflineExecutor::new(
    ProtocolVersion::MAX,
    1000,  // reference_gas_price (arbitrary for dev-inspect)
    0,     // epoch_id
    0,     // epoch_timestamp_ms
    store,
)?;

let tx_bytes_b64 = "AAABAAQDAA..."; // your transaction bytes
let tx_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_bytes_b64)?;
let tx: TransactionData = bcs::from_bytes(&tx_bytes)?;

let result = executor.simulate_transaction(tx, VmChecks::Disabled)?;

println!("status: {:?}", result.effects.status());
if let Ok(cmd_results) = &result.execution_result {
    for (i, (_muts, returns)) in cmd_results.iter().enumerate() {
        for (j, (bytes, type_tag)) in returns.iter().enumerate() {
            println!("command[{i}] return[{j}]: {type_tag} = {bytes:?}");
        }
    }
}
```

## Against a live node

If you need object state from a real chain, swap `OfflineExecutor` for `JsonRpcExecutor` / `GrpcExecutor` / `GraphqlExecutor`. The rest of the workflow is identical; the networked executor fetches objects transparently.

## Examples

- [../examples/offline_dev_inspect.rs](../examples/offline_dev_inspect.rs) — framework-only pure function call (blake2b256), no objects
- [../examples/local_dev_inspect.rs](../examples/local_dev_inspect.rs) — same call via JSON-RPC against a live endpoint
- [../examples/local_stake_inspect.rs](../examples/local_stake_inspect.rs) — staking transaction with shared + owned objects
