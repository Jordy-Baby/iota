// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Local Move VM executor for dry-run/dev-inspect without a node.
//!
//! This crate allows running Move VM transactions locally by fetching required
//! objects from a remote node (via JSON-RPC) and executing the transaction
//! using the same execution engine as a full node.
//!
//! Two executor types are provided:
//! - [`LocalExecutor`]: Fetches objects from a remote node on demand.
//! - [`OfflineExecutor`]: Uses only pre-provided objects, no network access.

mod execution;
mod in_memory_store;
mod local_executor;
mod offline_executor;
mod remote_store;

pub use in_memory_store::InMemoryStore;
pub use iota_types::transaction_executor::VmChecks;
pub use local_executor::LocalExecutor;
pub use offline_executor::OfflineExecutor;
pub use remote_store::RemoteStore;
