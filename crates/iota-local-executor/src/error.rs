// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Structured error type for failures the local executor can detect itself.
//!
//! Currently only pre-execution validation produces a tagged error
//! ([`LocalExecError::Validation`]) — bad signature, malformed transaction
//! data, gas budget below minimum, denied object, etc. Failures originating in
//! the remote fetch layer (JSON-RPC / gRPC / GraphQL) and inside the Move VM
//! still propagate up as raw `anyhow::Error`; they are not yet tagged.
//!
//! Users who want to branch on the validation case can downcast:
//!
//! ```ignore
//! match result.downcast_ref::<LocalExecError>() {
//!     Some(LocalExecError::Validation { .. }) => report_user_error(),
//!     _                                       => other_handler(),
//! }
//! ```

use std::fmt;

/// Structured error for failures the local executor classifies itself.
///
/// Today this only covers pre-execution validation — see module docs.
#[derive(Debug)]
pub enum LocalExecError {
    /// Pre-execution validation of the transaction failed — bad signature,
    /// malformed data, gas budget below minimum, denied object, etc.
    Validation {
        context: String,
        source: anyhow::Error,
    },
}

impl LocalExecError {
    pub fn validation(context: impl Into<String>, source: impl Into<anyhow::Error>) -> Self {
        Self::Validation {
            context: context.into(),
            source: source.into(),
        }
    }

    /// `true` if this error originated in pre-execution validation.
    pub fn is_validation(&self) -> bool {
        matches!(self, Self::Validation { .. })
    }
}

impl fmt::Display for LocalExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { context, source } => {
                write!(f, "[validation] {context}: {source}")
            }
        }
    }
}

impl std::error::Error for LocalExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation { source, .. } => source.source(),
        }
    }
}
