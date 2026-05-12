// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! Shared implementation surface for the two networked executors
//! (`GrpcExecutor`, `GraphqlExecutor`).
//!
//! Each networked executor wraps a transport-specific client + a
//! [`CachingStore`](crate::caching_store::CachingStore) populated by a
//! transport-specific fetcher, but the resulting public surface — fetch-mode
//! getters/setters, the `store` / `new_store` / `clear_cache` triple, event
//! decoding, and the eight `simulate*` methods — is byte-identical across
//! backends.
//!
//! Rather than duplicate those methods, the per-backend `impl` blocks call
//! [`networked_executor_methods!`] with their transport types as parameters
//! and the macro expands the shared surface in place. Each executor keeps its
//! own constructor (`new`, `with_debug` — the part that needs
//! transport-specific `ChainInfo` fetching) and prefetch logic (the part that
//! issues actual network calls).

/// Emit the shared method surface for a networked executor (`store`,
/// `new_store`, `clear_cache`, `decode_events`, `set_fetch_mode`,
/// `with_fetch_mode`, plus the six `simulate*` methods).
///
/// Arguments:
///
/// - `$Client` — the transport client type (e.g. `iota_grpc_client::Client`).
/// - `$Fetcher` — the tuple-struct fetcher (e.g. `GrpcFetcher`), expected to be
///   constructible as `$Fetcher(client.clone())`.
/// - `$Store` — the concrete `CachingStore` alias (e.g. `GrpcStore`).
///
/// The executor type must:
///
/// - Have fields `client: $Client`, `env: ExecutionEnv`, `fetch_mode:
///   ObjectFetchMode`, `shared_store: $Store`.
/// - Define an inherent `async fn prefetch_objects(&self, store: &$Store, tx:
///   &TransactionData) -> Result<()>`.
macro_rules! networked_executor_methods {
    ($Client:ty, $Fetcher:path, $Store:ty) => {
        /// Create the executor using a pre-fetched [`crate::ChainInfo`].
        /// Skips the network round-trip for protocol/epoch/gas-price — useful
        /// when building many executors against the same chain state.
        pub fn with_chain_info(client: $Client, info: $crate::ChainInfo) -> ::anyhow::Result<Self> {
            Self::with_chain_info_and_debug(client, info, $crate::DebugConfig::default())
        }

        /// [`Self::with_chain_info`] + a caller-supplied [`crate::DebugConfig`].
        pub fn with_chain_info_and_debug(
            client: $Client,
            info: $crate::ChainInfo,
            debug_config: $crate::DebugConfig,
        ) -> ::anyhow::Result<Self> {
            let env = $crate::execution::ExecutionEnv::with_debug(
                info.protocol_version,
                info.reference_gas_price,
                info.epoch_id,
                info.epoch_timestamp_ms,
                debug_config,
            )?;
            let shared_store = <$Store>::new($Fetcher(client.clone()));
            Ok(Self {
                client,
                env,
                fetch_mode: $crate::caching_store::ObjectFetchMode::default(),
                shared_store,
            })
        }

        /// Set the object fetch mode.
        pub fn set_fetch_mode(&mut self, mode: $crate::caching_store::ObjectFetchMode) {
            self.fetch_mode = mode;
        }

        /// Builder-style setter for the object fetch mode.
        #[must_use]
        pub fn with_fetch_mode(mut self, mode: $crate::caching_store::ObjectFetchMode) -> Self {
            self.fetch_mode = mode;
            self
        }

        /// Reference to the executor's persistent store. Useful for
        /// installing a [`crate::LocalPackage`] override via
        /// `install_into_caching` before the first simulation.
        pub fn store(&self) -> &$Store {
            &self.shared_store
        }

        /// Build a fresh, isolated [`$Store`] that shares this executor's
        /// client. Pair with `simulate_transaction_with_debug_using` for runs
        /// that must not observe or pollute the persistent cache.
        pub fn new_store(&self) -> $Store {
            <$Store>::new($Fetcher(self.client.clone()))
        }

        /// Drop every cached object and package override. The next simulation
        /// will fetch from the remote again.
        pub fn clear_cache(&self) {
            self.shared_store.clear();
        }

        /// Decode a `TransactionEvents` payload into fully-annotated
        /// [`crate::DecodedEvent`]s using this executor's Move VM type-layout
        /// resolver and persistent object cache.
        pub fn decode_events(
            &self,
            events: &::iota_types::effects::TransactionEvents,
        ) -> Vec<::anyhow::Result<$crate::DecodedEvent>> {
            $crate::execution::decode_events_with(&self.env, &self.shared_store, events)
        }

        /// Simulate a transaction locally against the executor's persistent
        /// store cache.
        ///
        /// - `VmChecks::Enabled` → dry-run mode (full checks, like a real transaction).
        /// - `VmChecks::Disabled` → dev-inspect mode (relaxed Move VM checks).
        ///
        /// **Caching:** fetched packages and objects persist across calls.
        /// Use [`Self::clear_cache`] to drop them if staleness matters, or
        /// [`Self::simulate_transaction_with_debug_using`] with a fresh
        /// [`Self::new_store`] for isolated runs.
        ///
        /// **Note on shared objects:** Shared objects are always fetched at
        /// the latest version because their version is assigned by consensus
        /// and is not encoded in the transaction. This means the local
        /// simulation may see a different shared-object state than the
        /// network would ultimately use when the transaction is executed for
        /// real.
        pub async fn simulate_transaction(
            &self,
            transaction: ::iota_types::transaction::TransactionData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<::iota_types::transaction_executor::SimulateTransactionResult> {
            self.prefetch_objects(&self.shared_store, &transaction)
                .await?;
            $crate::execution::simulate(&self.env, &self.shared_store, transaction, checks)
        }

        /// Simulate a transaction and return the captured [`crate::DebugArtifacts`]
        /// alongside the result. With a default [`crate::DebugConfig`] the
        /// artifacts are empty and this is equivalent to
        /// [`Self::simulate_transaction`].
        pub async fn simulate_transaction_with_debug(
            &self,
            transaction: ::iota_types::transaction::TransactionData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<$crate::DebugSimulateResult> {
            self.prefetch_objects(&self.shared_store, &transaction)
                .await?;
            $crate::execution::simulate_with_debug(
                &self.env,
                &self.shared_store,
                transaction,
                checks,
            )
        }

        /// Variant of [`Self::simulate_transaction_with_debug`] that reuses a
        /// caller-supplied store — lets the caller pre-install a
        /// [`crate::LocalPackage`] via `install_into_caching` before
        /// simulating.
        pub async fn simulate_transaction_with_debug_using(
            &self,
            store: $Store,
            transaction: ::iota_types::transaction::TransactionData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<$crate::DebugSimulateResult> {
            self.prefetch_objects(&store, &transaction).await?;
            $crate::execution::simulate_with_debug(&self.env, &store, transaction, checks)
        }

        /// Simulate a **signed** transaction locally, verifying signatures
        /// first.
        ///
        /// For standard schemes (Ed25519, Secp256k1, Secp256r1, MultiSig) the
        /// full cryptographic check runs before execution. For
        /// `MoveAuthenticator` signatures the sender address is checked
        /// upfront, then the authenticator function is executed inside the
        /// Move VM — this is the only way to fully validate
        /// `MoveAuthenticator` signatures.
        pub async fn simulate_signed_transaction(
            &self,
            signed_data: ::iota_types::transaction::SenderSignedData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<::iota_types::transaction_executor::SimulateTransactionResult> {
            self.prefetch_objects(&self.shared_store, signed_data.transaction_data())
                .await?;
            $crate::execution::simulate_signed(&self.env, &self.shared_store, signed_data, checks)
        }

        /// Signed-transaction variant of
        /// [`Self::simulate_transaction_with_debug`].
        pub async fn simulate_signed_transaction_with_debug(
            &self,
            signed_data: ::iota_types::transaction::SenderSignedData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<$crate::DebugSimulateResult> {
            self.prefetch_objects(&self.shared_store, signed_data.transaction_data())
                .await?;
            $crate::execution::simulate_signed_with_debug(
                &self.env,
                &self.shared_store,
                signed_data,
                checks,
            )
        }

        /// Variant of [`Self::simulate_signed_transaction_with_debug`] that
        /// reuses a caller-supplied store.
        pub async fn simulate_signed_transaction_with_debug_using(
            &self,
            store: $Store,
            signed_data: ::iota_types::transaction::SenderSignedData,
            checks: ::iota_types::transaction_executor::VmChecks,
        ) -> ::anyhow::Result<$crate::DebugSimulateResult> {
            self.prefetch_objects(&store, signed_data.transaction_data())
                .await?;
            $crate::execution::simulate_signed_with_debug(&self.env, &store, signed_data, checks)
        }
    };
}

pub(crate) use networked_executor_methods;
