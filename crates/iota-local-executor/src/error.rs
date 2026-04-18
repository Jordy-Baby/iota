// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Error types that separate the three places things can go wrong in a
//! local execution:
//!
//! - **Fetch**: talking to the remote node (JSON-RPC / gRPC / GraphQL transport
//!   error, missing object, BCS deserialisation of a fetched object).
//! - **Validation**: pre-execution checks on the transaction (bad signature,
//!   malformed transaction data, gas budget below minimum).
//! - **Execution**: the Move VM or execution engine rejecting the transaction
//!   during local simulation.
//!
//! A failed `simulate_transaction` today returns `anyhow::Error` without
//! hinting at which layer produced the failure. [`LocalExecError`] tags the
//! underlying cause and also converts cleanly to/from `anyhow::Error` so
//! existing call sites don't break.
//!
//! All existing `Result<T>` APIs on the crate continue to return
//! `anyhow::Result<T>`, but users who want structured handling can downcast:
//!
//! ```ignore
//! match result.downcast_ref::<LocalExecError>() {
//!     Some(LocalExecError::Fetch { .. })     => retry_with_backoff(),
//!     Some(LocalExecError::Validation { .. }) => report_user_error(),
//!     Some(LocalExecError::Execution { .. })  => inspect_effects(),
//!     _                                       => other_handler(),
//! }
//! ```

use std::fmt;

/// Structured error describing **where** a local execution failed. Attaches
/// the original `anyhow::Error` as the source so `{:#}` prints the full
/// chain.
#[derive(Debug)]
pub enum LocalExecError {
    /// The remote node (JSON-RPC / gRPC / GraphQL) returned an error or a
    /// malformed response while the executor was fetching an object.
    Fetch {
        context: String,
        source: anyhow::Error,
    },
    /// Pre-execution validation of the transaction failed — bad signature,
    /// malformed data, gas budget below minimum, denied object, etc.
    Validation {
        context: String,
        source: anyhow::Error,
    },
    /// The Move VM or execution engine rejected the transaction during
    /// simulation. The original `anyhow::Error` typically wraps an
    /// `ExecutionError` or an I/O error raised while loading from the store.
    Execution {
        context: String,
        source: anyhow::Error,
    },
}

impl LocalExecError {
    pub fn fetch(context: impl Into<String>, source: impl Into<anyhow::Error>) -> Self {
        Self::Fetch {
            context: context.into(),
            source: source.into(),
        }
    }
    pub fn validation(context: impl Into<String>, source: impl Into<anyhow::Error>) -> Self {
        Self::Validation {
            context: context.into(),
            source: source.into(),
        }
    }
    pub fn execution(context: impl Into<String>, source: impl Into<anyhow::Error>) -> Self {
        Self::Execution {
            context: context.into(),
            source: source.into(),
        }
    }

    /// `true` if this error originated in the remote-fetch layer.
    pub fn is_fetch(&self) -> bool {
        matches!(self, Self::Fetch { .. })
    }

    /// `true` if this error originated in pre-execution validation.
    pub fn is_validation(&self) -> bool {
        matches!(self, Self::Validation { .. })
    }

    /// `true` if this error originated in the Move VM or execution engine.
    pub fn is_execution(&self) -> bool {
        matches!(self, Self::Execution { .. })
    }
}

impl fmt::Display for LocalExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch { context, source } => {
                write!(f, "[fetch] {context}: {source}")
            }
            Self::Validation { context, source } => {
                write!(f, "[validation] {context}: {source}")
            }
            Self::Execution { context, source } => {
                write!(f, "[execution] {context}: {source}")
            }
        }
    }
}

impl std::error::Error for LocalExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch { source, .. }
            | Self::Validation { source, .. }
            | Self::Execution { source, .. } => source.source(),
        }
    }
}
