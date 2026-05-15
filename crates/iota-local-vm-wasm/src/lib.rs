// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Browser-friendly transaction simulation. The JS side fetches input objects
//! from a public IOTA RPC; the Rust/WASM side decodes the BCS transaction and
//! runs it through the full Move VM with the supplied objects.

pub mod executor;
pub mod store;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use executor::OfflineExecutor;
pub use store::InMemoryStore;
