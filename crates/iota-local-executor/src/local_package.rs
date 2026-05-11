// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Compile a Move source package in-process and expose it to the local
//! executor under a caller-chosen synthetic [`ObjectID`], without ever
//! publishing it on-chain.
//!
//! One-line summary: `LocalPackage::compile(path, 0x42, "hello",
//! &protocol_config)` gives you an [`Object`] that can be `install_into(...)`
//! a store so that any PTB referencing `0x42::<module>::<fn>` resolves against
//! the freshly compiled bytecode.
//!
//! Reuses the existing `iota-move-build` pipeline (including
//! `BuildConfig::new_for_testing_replace_addresses`) rather than
//! reimplementing compilation, so framework linkage, type-origin tables, and
//! bytecode verification all come for free.

use std::path::Path;

use anyhow::{Result, anyhow};
use iota_framework::BuiltInFramework;
use iota_move_build::{BuildConfig, CompiledPackage};
use iota_protocol_config::ProtocolConfig;
use iota_types::{
    base_types::ObjectID, digests::TransactionDigest, move_package::MovePackage, object::Object,
};
use move_binary_format::CompiledModule;
use move_core_types::account_address::AccountAddress;

use crate::{InMemoryStore, caching_store::CachingStore};

/// A locally-compiled Move package wired to a caller-chosen synthetic
/// [`ObjectID`].
///
/// Built via [`LocalPackage::compile`]. Install into a store with
/// [`LocalPackage::install_into`] (for [`InMemoryStore`]) or
/// [`LocalPackage::install_into_caching`] (for any [`CachingStore`]).
pub struct LocalPackage {
    /// The synthetic package ID the caller chose (e.g. `0x42`). Both the
    /// compiled bytecode and the resulting `MovePackage` report this ID.
    pub id: ObjectID,
    /// The on-chain package object ready to be inserted into a store.
    pub object: Object,
    /// The full [`CompiledPackage`] kept for Phase 3 source-map use. The
    /// bytecode already lives inside `object`; this field is not required for
    /// execution.
    pub compiled: CompiledPackage,
}

impl LocalPackage {
    /// Compile a Move source package at `path` and bind its declared
    /// `named_address` to `synthetic_id`, producing an [`Object`] that can be
    /// installed in any [`InMemoryStore`] or [`CachingStore`].
    ///
    /// `named_address` must match the name declared under `[addresses]` in
    /// the package's `Move.toml` (e.g. `hello` for `hello = "0x0"`).
    /// `protocol_config` must match the protocol version the executor runs
    /// with — the framework layout is coupled to it.
    pub fn compile(
        path: &Path,
        synthetic_id: ObjectID,
        named_address: &str,
        protocol_config: &ProtocolConfig,
    ) -> Result<Self> {
        let build_config = BuildConfig::new_for_testing_replace_addresses([(
            named_address.to_string(),
            synthetic_id,
        )]);
        let compiled = build_config
            .build(path)
            .map_err(|e| anyhow!("compiling local package at {path:?}: {e}"))?;

        let modules: Vec<CompiledModule> = compiled.get_modules().cloned().collect();
        if modules.is_empty() {
            return Err(anyhow!(
                "local package at {path:?} compiled to zero modules — check the package layout"
            ));
        }
        let expected = AccountAddress::new(synthetic_id.into_bytes());
        for module in &modules {
            if module.address() != &expected {
                return Err(anyhow!(
                    "compiled module {} has address {}, expected synthetic ID {} — \
                     check that `[addresses]` in Move.toml declares `{}` and that \
                     the source uses it",
                    module.self_id().name(),
                    module.address(),
                    synthetic_id,
                    named_address
                ));
            }
        }

        // Framework packages supply the `MovePackage` dependencies that
        // `new_package` threads through `linkage_table` construction.
        let dependencies: Vec<MovePackage> = BuiltInFramework::genesis_move_packages().collect();
        let object = Object::new_package(
            &modules,
            TransactionDigest::ZERO,
            protocol_config,
            &dependencies,
        )
        .map_err(|e| anyhow!("assembling MovePackage for {synthetic_id}: {e}"))?;

        Ok(Self {
            id: synthetic_id,
            object,
            compiled,
        })
    }

    /// Install this package into an [`InMemoryStore`] for use by
    /// [`OfflineExecutor`](crate::OfflineExecutor).
    pub fn install_into(&self, store: &mut InMemoryStore) {
        store.insert(self.object.clone());
    }

    /// Install this package as an override in a [`CachingStore`], shadowing
    /// any remote fetch for [`Self::id`].
    pub fn install_into_caching<F>(&self, store: &CachingStore<F>) {
        store.insert_package_override(self.object.clone());
    }
}
