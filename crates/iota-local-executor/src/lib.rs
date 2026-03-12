// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Local Move VM executor for dry-run/dev-inspect without a node.
//!
//! This crate allows running Move VM transactions locally by fetching required
//! objects from a remote node (via JSON-RPC, gRPC, or GraphQL) and executing
//! the transaction using the same execution engine as a full node.
//!
//! Four executor types are provided:
//! - [`JsonRpcExecutor`]: Fetches objects via JSON-RPC.
//! - [`GrpcExecutor`]: Fetches objects via gRPC.
//! - [`GraphqlExecutor`]: Fetches objects via GraphQL.
//! - [`OfflineExecutor`]: Uses only pre-provided objects, no network access.

mod caching_store;
mod execution;
mod graphql_executor;
mod grpc_executor;
mod in_memory_store;
mod json_rpc_executor;
mod offline_executor;

pub use caching_store::{GraphqlStore, GrpcStore, JsonRpcStore};
pub use graphql_executor::GraphqlExecutor;
pub use grpc_executor::GrpcExecutor;
pub use in_memory_store::InMemoryStore;
pub use iota_types::transaction_executor::VmChecks;
pub use json_rpc_executor::{JsonRpcExecutor, ObjectFetchMode};
pub use offline_executor::OfflineExecutor;
