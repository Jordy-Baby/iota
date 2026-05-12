// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Local Move VM executor for dry-run/dev-inspect without a node.
//!
//! This crate allows running Move VM transactions locally by fetching required
//! objects from a remote node (via gRPC or GraphQL) and executing the
//! transaction using the same execution engine as a full node.
//!
//! Three executor types are provided:
//! - [`GrpcExecutor`]: Fetches objects via gRPC.
//! - [`GraphqlExecutor`]: Fetches objects via GraphQL.
//! - [`OfflineExecutor`]: Uses only pre-provided objects, no network access.

mod caching_store;
mod chain_info;
mod debug;
mod error;
mod events;
mod execution;
mod gas;
mod graphql_executor;
mod grpc_executor;
mod ide;
mod in_memory_store;
mod local_package;
mod multi_tx;
mod networked_helpers;
mod offline_executor;

pub use caching_store::{GraphqlStore, GrpcStore, ObjectFetchMode};
pub use chain_info::ChainInfo;
pub use debug::{DebugArtifacts, DebugConfig, DebugSimulateResult, ProfileOutput, ProfileSink};
pub use error::LocalExecError;
pub use events::{DecodedEvent, decode_events};
pub use gas::GasEstimate;
pub use graphql_executor::GraphqlExecutor;
pub use grpc_executor::GrpcExecutor;
pub use ide::{FunctionGasSummary, SourceSpan, summarize_by_function, summarize_with_source};
pub use in_memory_store::InMemoryStore;
// Pass-through re-exports of widely-used types from `iota_types`. They keep
// `iota_local_executor::VmChecks` / `SenderSignedData` working without forcing
// callers to depend on `iota_types` directly.
pub use iota_types::{transaction::SenderSignedData, transaction_executor::VmChecks};
pub use local_package::LocalPackage;
pub use multi_tx::{
    ChainHistory, ChainedOfflineExecutor, apply_effects_to_caching, apply_effects_to_in_memory,
};
pub use offline_executor::OfflineExecutor;
