// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Chain multiple simulated transactions so that each one sees the state
//! changes produced by the previous.
//!
//! The Move VM isn't stateful between `simulate_*` calls — every run reads
//! from whatever `BackingStore` it's given and drops the resulting
//! [`InnerTemporaryStore`](iota_types::inner_temporary_store::InnerTemporaryStore)
//! on the floor. Chaining is therefore a matter of **applying the effects of
//! tx N back into the store before running tx N+1**:
//!
//! - Every `Object` in `SimulateTransactionResult::output_objects` is
//!   re-inserted (created or mutated state).
//! - Every `ObjectID` in `TransactionEffects::deleted()` / `wrapped()` is
//!   removed from the store.
//!
//! This module exposes two thin helpers:
//!
//! - [`apply_effects_to_in_memory`] / [`apply_effects_to_caching`] — one
//!   function per store type, each applies the created/mutated/deleted set in
//!   the right order.
//! - [`ChainedOfflineExecutor`] — a convenience wrapper around
//!   [`OfflineExecutor`] that applies effects automatically after every
//!   successful simulate call and remembers the accumulated [`EndState`].
//!
//! Gas: if a simulated transaction has an empty gas-payment the executor
//! mints a one-shot mock gas coin (see `mock_gas_id` in the result). The
//! mock coin is **not** persisted into the store by default — the next
//! transaction in the chain will get its own fresh mock. Callers who want
//! realistic gas bookkeeping across a chain should supply an explicit gas
//! coin up front.

use anyhow::Result;
use iota_types::{
    base_types::ObjectID,
    effects::{TransactionEffects, TransactionEffectsAPI},
    transaction::TransactionData,
    transaction_executor::{SimulateTransactionResult, VmChecks},
};

use crate::{
    DebugSimulateResult, InMemoryStore, OfflineExecutor,
    caching_store::{CachingStore, ObjectFetcher},
};

/// Apply the created/mutated/deleted/wrapped object changes from a single
/// simulation into an [`InMemoryStore`] so subsequent simulate calls see
/// them.
pub fn apply_effects_to_in_memory(store: &mut InMemoryStore, result: &SimulateTransactionResult) {
    // Write all created and mutated objects back. Insertion also covers
    // `unwrapped` objects — they are present in `output_objects` with their
    // new owner.
    for (_id, obj) in &result.output_objects {
        store.insert(obj.clone());
    }
    // Remove deleted and wrapped objects. Wrapped objects are still
    // reachable on-chain via their wrapper, but they should no longer be
    // looked up by the wrapped ID directly.
    for (id, _, _) in result.effects.deleted() {
        store.remove(&id);
    }
    for (id, _, _) in result.effects.wrapped() {
        store.remove(&id);
    }
    // Don't persist the mock gas coin — see module-level note.
    if let Some(id) = result.mock_gas_id {
        store.remove(&id);
    }
}

/// Apply effects into a [`CachingStore`]. Identical semantics to
/// [`apply_effects_to_in_memory`] but uses the interior-mutability store API.
pub fn apply_effects_to_caching<F: ObjectFetcher>(
    store: &CachingStore<F>,
    result: &SimulateTransactionResult,
) {
    for (_id, obj) in &result.output_objects {
        store.insert(obj.clone());
    }
    for (id, _, _) in result.effects.deleted() {
        store.remove(&id);
    }
    for (id, _, _) in result.effects.wrapped() {
        store.remove(&id);
    }
    if let Some(id) = result.mock_gas_id {
        store.remove(&id);
    }
}

/// Accumulated bookkeeping exposed by [`ChainedOfflineExecutor`]. One entry
/// per successful simulation in the chain.
///
/// Only [`TransactionEffects`] are retained per step — the full
/// `SimulateTransactionResult` isn't `Clone` (it transitively holds
/// `ExecutionError`, which is not `Clone` either). Effects carry everything
/// the caller typically needs from history: created/mutated/deleted refs,
/// gas summary, status.
#[derive(Default)]
pub struct ChainHistory {
    pub steps: Vec<TransactionEffects>,
}

impl ChainHistory {
    /// IDs of every object created across the entire chain so far — useful
    /// when the caller needs to reference a newly-created object in a later
    /// transaction.
    pub fn created_object_ids(&self) -> impl Iterator<Item = ObjectID> + '_ {
        self.steps
            .iter()
            .flat_map(|effects| effects.created().into_iter().map(|(objref, _)| objref.0))
    }
}

/// An [`OfflineExecutor`] wrapper that applies each transaction's effects
/// back into its own store so subsequent simulations in the chain see them.
///
/// Build via [`ChainedOfflineExecutor::new`] from an existing
/// [`OfflineExecutor`]. Call [`Self::simulate`] (or
/// [`Self::simulate_with_debug`]) as many times as you like — the underlying
/// store accumulates state in order.
pub struct ChainedOfflineExecutor {
    inner: OfflineExecutor,
    history: ChainHistory,
}

impl ChainedOfflineExecutor {
    pub fn new(executor: OfflineExecutor) -> Self {
        Self {
            inner: executor,
            history: ChainHistory::default(),
        }
    }

    /// Simulate the next transaction in the chain. After a successful run
    /// its effects are applied to the store before returning, so the next
    /// call sees the new state.
    pub fn simulate(
        &mut self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<SimulateTransactionResult> {
        let result = self.inner.simulate_transaction(transaction, checks)?;
        apply_effects_to_in_memory(self.inner.store_mut(), &result);
        self.history.steps.push(result.effects.clone());
        Ok(result)
    }

    /// Debug-enabled variant of [`Self::simulate`]. Returns the full
    /// [`DebugSimulateResult`] so the caller can read artifacts; the
    /// transaction's effects are also appended to [`Self::history`].
    pub fn simulate_with_debug(
        &mut self,
        transaction: TransactionData,
        checks: VmChecks,
    ) -> Result<DebugSimulateResult> {
        let out = self
            .inner
            .simulate_transaction_with_debug(transaction, checks)?;
        apply_effects_to_in_memory(self.inner.store_mut(), &out.result);
        self.history.steps.push(out.result.effects.clone());
        Ok(out)
    }

    /// Release ownership of the underlying executor — useful to keep using
    /// the now-fully-populated store for standalone calls afterwards.
    pub fn into_inner(self) -> OfflineExecutor {
        self.inner
    }

    /// Accumulated per-step effects. Index 0 is the first call.
    pub fn history(&self) -> &ChainHistory {
        &self.history
    }
}
