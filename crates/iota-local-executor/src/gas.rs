// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Gas-estimation helpers built on top of a simulation's effects.
//!
//! The Move VM already records the full gas ledger for every simulated
//! transaction — computation cost, storage cost, and storage rebate — inside
//! the returned
//! [`TransactionEffects::gas_cost_summary`](iota_types::effects::TransactionEffectsAPI::gas_cost_summary).
//! The types below are just convenience shapes that surface the numbers a
//! typical dev-inspect caller actually wants:
//!
//! - [`GasEstimate::from_effects`] extracts the summary and computes `net =
//!   computation + storage - rebate`, the figure you'd normally compare against
//!   a gas budget.
//! - [`GasEstimate::suggested_budget_with_headroom`] returns a proposed budget
//!   value (`net * headroom_factor`) callers can plug into the next
//!   `TransactionData::new_programmable(...)` call.

use iota_types::{effects::TransactionEffectsAPI, gas::GasCostSummary};

/// Convenience summary of a simulation's gas ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasEstimate {
    pub computation_cost: u64,
    pub storage_cost: u64,
    pub storage_rebate: u64,
    pub non_refundable_storage_fee: u64,
    /// `computation_cost + storage_cost - storage_rebate`. The figure a gas
    /// budget needs to cover.
    pub net_gas_usage: i64,
}

impl GasEstimate {
    /// Build an estimate from any type exposing the
    /// [`TransactionEffectsAPI`].
    pub fn from_effects<E: TransactionEffectsAPI>(effects: &E) -> Self {
        let summary = effects.gas_cost_summary();
        Self::from_summary(summary)
    }

    pub fn from_summary(s: &GasCostSummary) -> Self {
        Self {
            computation_cost: s.computation_cost,
            storage_cost: s.storage_cost,
            storage_rebate: s.storage_rebate,
            non_refundable_storage_fee: s.non_refundable_storage_fee,
            net_gas_usage: s.net_gas_usage(),
        }
    }

    /// Suggest a gas budget for a future execution of the same transaction.
    /// Multiplies [`Self::net_gas_usage`] by `headroom_factor` and rounds
    /// up. `headroom_factor = 1.2` (20% safety margin) is a reasonable
    /// default.
    ///
    /// Returns `0` if the simulation reported a net **rebate** (i.e. a
    /// storage-refund-heavy tx) — gas budgets are always `>= 0`.
    pub fn suggested_budget_with_headroom(&self, headroom_factor: f64) -> u64 {
        if self.net_gas_usage <= 0 {
            return 0;
        }
        let raw = (self.net_gas_usage as f64) * headroom_factor;
        raw.ceil() as u64
    }
}
