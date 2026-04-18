# Chained (multi-transaction) execution

Simulate a sequence of transactions where each one sees the state changes from the previous — useful for testing flows like "publish, then instantiate, then interact," or for reproducing a bug that depends on prior on-chain history.

## How it works

The Move VM isn't stateful between `simulate_*` calls: each run reads from whatever `BackingStore` it's given and drops the resulting temporary store. Chaining is a matter of **applying the effects of tx N back into the store before running tx N+1**:

- Every `Object` in `SimulateTransactionResult::output_objects` is re-inserted into the store (created + mutated + unwrapped).
- Every `ObjectID` in `TransactionEffects::deleted()` / `wrapped()` is removed.
- The mock gas coin (if the tx had no explicit gas payment) is **not** persisted — the next tx in the chain gets its own fresh mock.

`ChainedOfflineExecutor` wraps an `OfflineExecutor` and does this automatically after every call.

## End-to-end

```rust
use iota_local_executor::{
    ChainedOfflineExecutor, InMemoryStore, LocalPackage, OfflineExecutor, VmChecks,
};
use iota_types::{
    base_types::IotaAddress,
    effects::TransactionEffectsAPI,
    object::Owner,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{ObjectArg, TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData},
};
use move_core_types::ident_str;

let offline = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;
let mut chain = ChainedOfflineExecutor::new(offline);

// tx1: create a shared counter
let mut b = ProgrammableTransactionBuilder::new();
b.programmable_move_call(
    synthetic_id,
    ident_str!("counter").into(),
    ident_str!("create").into(),
    vec![], vec![],
);
let tx1 = TransactionData::new_programmable(
    IotaAddress::ZERO, vec![], b.finish(),
    TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, 1000,
);
let r1 = chain.simulate(tx1, VmChecks::Enabled)?;

// Find the newly-created shared counter
let (counter_ref, initial_shared_version) = r1.effects.created().into_iter()
    .find_map(|(oref, owner)| match owner {
        Owner::Shared { initial_shared_version } => Some((oref, initial_shared_version)),
        _ => None,
    })
    .unwrap();

// tx2: increment it — this works because tx1's effects are now in the store
let mut b = ProgrammableTransactionBuilder::new();
let arg = b.obj(ObjectArg::SharedObject {
    id: counter_ref.0,
    initial_shared_version,
    mutable: true,
}).unwrap();
b.programmable_move_call(
    synthetic_id,
    ident_str!("counter").into(),
    ident_str!("increment").into(),
    vec![], vec![arg],
);
let tx2 = TransactionData::new_programmable(
    IotaAddress::ZERO, vec![], b.finish(),
    TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, 1000,
);
let r2 = chain.simulate(tx2, VmChecks::Enabled)?;
assert!(r2.effects.status().is_ok());
```

## History

Each successful call pushes the step's `TransactionEffects` onto `chain.history()`. `created_object_ids()` iterates every object created across the entire chain so far — handy for discovering the ID of something tx1 minted that tx3 wants to reference.

```rust
for created_id in chain.history().created_object_ids() {
    println!("chain created: {created_id}");
}
```

## Manual mode

If you want more control than `ChainedOfflineExecutor`, use `apply_effects_to_in_memory` directly:

```rust
use iota_local_executor::apply_effects_to_in_memory;

let r1 = offline.simulate_transaction(tx1, VmChecks::Enabled)?;
apply_effects_to_in_memory(offline.store_mut(), &r1);
// store now has everything tx1 created/mutated — run tx2 against it.
```

For networked executors, use `apply_effects_to_caching(&executor.store(), &result)` — the caching stores have interior mutability so no `&mut` is needed.

## Caveats

- **Shared-object versions.** In dry-run (`VmChecks::Enabled`), shared-object versions are normally assigned by consensus. For chained simulations, the VM bumps them sequentially — fine for testing flows, but differs from production.
- **Gas coin.** If you need realistic gas bookkeeping across a chain (e.g. to verify a sender can afford all txs), supply an explicit gas coin in every tx rather than relying on the mock.
- **`VmChecks::Disabled` (dev-inspect).** Works but is less meaningful for chains — dev-inspect skips most validation, so tx2 won't catch "you're trying to mutate an object tx1 never created."

## Examples and tests

- [../../tests/multi_tx.rs::chain_create_then_increment_counter](../../tests/multi_tx.rs) — the exact flow above
- [../../tests/multi_tx.rs::manual_apply_effects_across_calls](../../tests/multi_tx.rs) — `apply_effects_to_in_memory` directly
