# Event decoding

Decode the raw BCS-encoded events attached to a simulation result into fully-annotated `MoveValue`s — named fields, typed, ready to pattern-match on.

## The raw shape

`SimulateTransactionResult::events` is an `Option<TransactionEvents>` where each `Event` carries:

- `package_id`, `transaction_module`, `sender`, `type_: StructTag` — the metadata
- `contents: Vec<u8>` — BCS-encoded payload with no field names

Decoding needs the `StructTag`'s layout, which in turn needs to load the containing Move package. `executor.decode_events(&events)` does all of that using the executor's own Move VM type-layout resolver and persistent object cache.

## Emit an event from Move

```move
module hello::counter {
    use iota::event;

    public struct Incremented has copy, drop {
        new_value: u64,
    }

    public entry fun increment(counter: &mut Counter) {
        counter.value = counter.value + 1;
        event::emit(Incremented { new_value: counter.value });
    }
}
```

## Decode

```rust
use iota_local_executor::{OfflineExecutor, VmChecks};
use move_core_types::annotated_value::MoveValue;

let result = executor.simulate_transaction(tx, VmChecks::Enabled)?;
let events = result.events.as_ref().unwrap();

for decoded in executor.decode_events(events) {
    let event = decoded?;
    println!(
        "event {}::{} from {}:",
        event.package_id, event.transaction_module, event.sender
    );
    match &event.value {
        MoveValue::Struct(s) => {
            for (name, value) in &s.fields {
                println!("  {name} = {value:?}");
            }
        }
        other => println!("  {other:?}"),
    }
}
```

`decode_events` returns `Vec<Result<DecodedEvent>>` — one `Result` per event, so a single event with a missing layout doesn't mask the rest.

## Accessing specific fields

`MoveValue::Struct { fields: Vec<(Identifier, MoveValue)> }` is pattern-matchable directly. For typed deserialisation from the raw BCS, the `contents_bcs` field is retained on every `DecodedEvent` so you can do `bcs::from_bytes::<MyEventStruct>(&event.contents_bcs)` if you have a Rust mirror struct.

## Across executor backends

`decode_events` works identically on `OfflineExecutor`, `GrpcExecutor`, and `GraphqlExecutor`. Each one uses its own object store for layout resolution. If a custom/non-framework package emitted the event, make sure that package is either installed into the store (`LocalPackage::install_into*`) or fetchable from the remote.

## Examples and tests

- [../tests/event_decoding.rs::decode_emitted_incremented_event](../tests/event_decoding.rs) — full round-trip: emit, decode, assert field name + value
- [../src/events.rs](../src/events.rs) — `DecodedEvent` shape, `decode_events` implementation
