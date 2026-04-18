// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

/// Stateful fixture used by the multi-transaction chaining test
/// (`tests/multi_tx.rs`). `create` yields a fresh shared counter object;
/// `increment` bumps it. Writing the counter back into the store between
/// calls is how the chained executor exercises its state-carryover code path.
module hello::counter {
    use iota::event;
    use iota::object::{Self, UID};
    use iota::transfer;
    use iota::tx_context::TxContext;

    public struct Counter has key, store {
        id: UID,
        value: u64,
    }

    /// Event emitted when a counter is incremented — exercised by the
    /// event-decoding test.
    public struct Incremented has copy, drop {
        new_value: u64,
    }

    /// Create a counter and share it so a subsequent transaction can take it
    /// as a shared-object input and mutate it.
    public entry fun create(ctx: &mut TxContext) {
        transfer::share_object(Counter { id: object::new(ctx), value: 0 });
    }

    /// Increment the shared counter in place and emit an `Incremented`
    /// event.
    public entry fun increment(counter: &mut Counter) {
        counter.value = counter.value + 1;
        event::emit(Incremented { new_value: counter.value });
    }

    public fun value(counter: &Counter): u64 {
        counter.value
    }
}
