# Local Move packages with synthetic IDs

Compile a Move source package in-process, bind its declared named address to an `ObjectID` you choose (e.g. `0x42`), and run transactions against it — without ever publishing to a network.

This is the foundation for the debug-print, gas-profiling, and tracing workflows: **you can't trace code that's only running on a remote node**, so the first step for any local debugging work is getting your package into a local store.

## Writing the Move package

A local package looks like any other Move package, with one difference: the named address in `Move.toml` is **unbound** so the executor can set it at build time.

```toml
# Move.toml
[package]
name = "HelloDebug"
version = "0.0.1"
edition = "2024"

[dependencies]
Iota = { local = "../iota-framework/packages/iota-framework" }

[addresses]
# Unbound — `LocalPackage::compile` supplies the synthetic ID at build time.
hello = "_"
iota = "0x2"
```

```move
// sources/hello.move
module hello::hello {
    use std::debug;

    public fun greet() {
        let msg = b"hello from iota-local-executor";
        debug::print(&msg);
    }

    public fun sum_to(n: u64): u64 {
        let mut total: u64 = 0;
        let mut i: u64 = 0;
        while (i < n) { total = total + i; i = i + 1; };
        total
    }
}
```

## Compile + install

```rust
use iota_framework::BuiltInFramework;
use iota_local_executor::{InMemoryStore, LocalPackage, OfflineExecutor, VmChecks};
use iota_protocol_config::{Chain, ProtocolConfig, ProtocolVersion};
use iota_types::{
    base_types::{IotaAddress, ObjectID},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::{TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE, TransactionData},
};
use move_core_types::ident_str;

let synthetic_id = ObjectID::from_hex_literal("0x42")?;
let protocol_config = ProtocolConfig::get_for_version(ProtocolVersion::MAX, Chain::Unknown);

// Compile: bytecode is produced with `hello -> 0x42` baked in.
let pkg = LocalPackage::compile(
    std::path::Path::new("path/to/hello_debug"),
    synthetic_id,
    "hello",         // the named-address key from Move.toml
    &protocol_config,
)?;

// Seed the store with framework + the compiled package.
let mut store = InMemoryStore::new();
for obj in BuiltInFramework::genesis_objects() {
    store.insert(obj);
}
pkg.install_into(&mut store);

let executor = OfflineExecutor::new(ProtocolVersion::MAX, 1000, 0, 0, store)?;
```

## Call into it from a PTB

```rust
let mut builder = ProgrammableTransactionBuilder::new();
let n = builder.pure(10u64)?;
builder.programmable_move_call(
    synthetic_id,
    ident_str!("hello").into(),
    ident_str!("sum_to").into(),
    vec![],
    vec![n],
);

// Empty gas payment → the executor mints a mock gas coin automatically.
let tx = TransactionData::new_programmable(
    IotaAddress::ZERO,
    vec![],
    builder.finish(),
    TEST_ONLY_GAS_UNIT_FOR_HEAVY_COMPUTATION_STORAGE,
    1000,
);
let result = executor.simulate_transaction(tx, VmChecks::Disabled)?;
assert!(result.effects.status().is_success());
```

## Mixing with remote objects

For mixed workflows (local synthetic package + remotely-fetched inputs), use a networked executor and `install_into_caching`:

```rust
let executor = JsonRpcExecutor::new(client).await?;
pkg.install_into_caching(executor.store());   // 0x42 shadows any remote
let result = executor.simulate_transaction(tx, VmChecks::Disabled).await?;
```

## Examples and tests

- [../examples/local_package_debug.rs](../examples/local_package_debug.rs) — full end-to-end demo (compile → execute → profile + trace)
- [../tests/local_package.rs](../tests/local_package.rs) — `compile_binds_synthetic_id_into_every_module`, `ptb_call_against_synthetic_package_succeeds`
- [../tests/fixtures/hello_debug/](../tests/fixtures/hello_debug/) — the fixture package used by every debug/tracing test
