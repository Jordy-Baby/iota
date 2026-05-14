// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

/// Coverage workload for consistent-views QA. Exposes a small surface of
/// dynamic-field / DOF / wrap / unwrap / lifecycle operations the QA driver
/// can call to populate a localnet with diverse object histories.
module workload::workload {
    use iota::dynamic_field as df;
    use iota::dynamic_object_field as dof;
    use std::option::{Self, Option};

    /// DF/DOF container, transferable and storable.
    public struct Parent has key, store {
        id: UID,
        value: u64,
    }

    /// Leaf object: can be a DOF child or wrapped inside Wrapper.
    public struct Child has key, store {
        id: UID,
        value: u64,
    }

    /// Wrapper that owns a Child inline. Used for wrap/unwrap scenarios.
    public struct Wrapper has key, store {
        id: UID,
        inner: Option<Child>,
    }

    // === mint / send-to-sender ===

    public entry fun mint_parent_to_sender(value: u64, ctx: &mut TxContext) {
        transfer::public_transfer(
            Parent { id: object::new(ctx), value },
            ctx.sender(),
        );
    }

    public entry fun mint_child_to_sender(value: u64, ctx: &mut TxContext) {
        transfer::public_transfer(
            Child { id: object::new(ctx), value },
            ctx.sender(),
        );
    }

    // === dynamic fields (primitive key/value) ===

    public entry fun add_df(parent: &mut Parent, key: u64, value: u64) {
        df::add(&mut parent.id, key, value);
    }

    public entry fun remove_df(parent: &mut Parent, key: u64) {
        let _: u64 = df::remove(&mut parent.id, key);
    }

    // === dynamic object fields (Child as value) ===

    public entry fun add_dof_to(
        parent: &mut Parent,
        key: u64,
        child_value: u64,
        ctx: &mut TxContext,
    ) {
        let child = Child { id: object::new(ctx), value: child_value };
        dof::add(&mut parent.id, key, child);
    }

    public entry fun remove_dof_and_delete(parent: &mut Parent, key: u64) {
        let child: Child = dof::remove(&mut parent.id, key);
        let Child { id, value: _ } = child;
        id.delete();
    }

    // === wrap / unwrap ===

    public entry fun wrap_self_and_send(child: Child, ctx: &mut TxContext) {
        let wrapper = Wrapper {
            id: object::new(ctx),
            inner: option::some(child),
        };
        transfer::public_transfer(wrapper, ctx.sender());
    }

    public entry fun unwrap_and_send(wrapper: Wrapper, ctx: &mut TxContext) {
        let Wrapper { id, mut inner } = wrapper;
        id.delete();
        let child = inner.extract();
        inner.destroy_none();
        transfer::public_transfer(child, ctx.sender());
    }

    /// Wrap + delete the wrapper (and the inner child along with it). The
    /// child ends in `WrappedOrDeleted` state — useful for tombstone
    /// coverage tests.
    public entry fun wrap_and_delete(child: Child, ctx: &mut TxContext) {
        let Wrapper { id, inner } = Wrapper {
            id: object::new(ctx),
            inner: option::some(child),
        };
        id.delete();
        // burn the wrapped child too
        let child = option::destroy_some(inner);
        let Child { id: child_id, value: _ } = child;
        child_id.delete();
    }

    // === plain delete ===

    public entry fun delete_parent(parent: Parent) {
        let Parent { id, value: _ } = parent;
        id.delete();
    }

    public entry fun delete_child(child: Child) {
        let Child { id, value: _ } = child;
        id.delete();
    }

    // === borrow_mut quirk: mutate a DOF child without bumping parent's version ===

    /// Adds a DF to a DOF child via `borrow_mut`. Increments the child's
    /// version but the parent's version is NOT bumped (this is the
    /// "borrow_mut quirk" that the consistency model has to handle).
    public entry fun add_df_to_dof_child(
        parent: &mut Parent,
        dof_key: u64,
        df_key: u64,
        df_value: u64,
    ) {
        let child: &mut Child = dof::borrow_mut(&mut parent.id, dof_key);
        df::add(&mut child.id, df_key, df_value);
    }
}
