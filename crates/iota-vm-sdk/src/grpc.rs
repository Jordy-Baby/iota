// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! gRPC-backed store (`feature = "grpc"`, native only).
//!
//! [`GrpcStore`] wraps an [`InMemoryStore`] cache and a gRPC [`Client`]. The
//! [`Store`] trait the executor uses is synchronous, but the Move VM resolves
//! objects on demand mid-execution, so a cache miss is served by blocking on
//! the client to fetch that object, then caching it. Only the objects a run
//! actually touches are fetched.
//! [`fetch_chain_context`](GrpcStore::fetch_chain_context) still runs up front,
//! and callers may [`prefetch`](GrpcStore::prefetch) to warm the cache, but
//! neither is required for correctness.
//!
//! On-demand fetching blocks the executor thread on async I/O via
//! [`block_in_place`], so [`LocalVm::execute`](crate::LocalVm::execute) must
//! run inside a multi-threaded Tokio runtime (e.g. `#[tokio::main]`).

use std::sync::{Arc, Mutex};

use iota_grpc_client::Client;
use iota_sdk_types::{ObjectId, Version};
use iota_types::{
    object::Object,
    transaction::{InputObjectKind, TransactionData, TransactionDataAPI},
};
use tokio::{runtime::Handle, task::block_in_place};

use crate::{
    error::{StoreError, VmSdkError},
    executor::ChainContext,
    store::{InMemoryStore, Store},
};

/// A [`Store`] backed by a remote node over gRPC, resolving objects on demand.
///
/// Clones share the same cache and client.
#[derive(Clone)]
pub struct GrpcStore {
    inner: Arc<Mutex<InMemoryStore>>,
    client: Client,
    last_fetch_error: Arc<Mutex<Option<String>>>,
}

impl GrpcStore {
    /// Wrap an existing client. The store starts with the built-in framework
    /// packages already loaded so Move calls resolve.
    pub fn new(client: Client) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InMemoryStore::with_framework())),
            client,
            last_fetch_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Connect to a gRPC endpoint (by URL) and create a store containing only
    /// the built-in framework packages.
    ///
    /// # Errors
    ///
    /// Returns [`VmSdkError::Store`] if the client cannot be created for `url`.
    pub fn connect(url: impl Into<String>) -> Result<Self, VmSdkError> {
        let url: String = url.into();
        let client = Client::new(url).map_err(|e| StoreError::new("connect gRPC", e))?;
        Ok(Self::new(client))
    }

    /// A snapshot clone of the objects cached so far (framework packages plus
    /// anything fetched on demand or pre-fetched).
    pub fn store(&self) -> InMemoryStore {
        self.inner.lock().expect("store lock poisoned").clone()
    }

    /// Fetch the chain parameters a [`LocalVm`](crate::LocalVm) needs.
    ///
    /// Reports [`Chain::Unknown`](crate::Chain) — the chain identity is not
    /// resolved here; this only affects chain-specific protocol behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`VmSdkError::Store`] if the epoch RPC fails or can't be
    /// decoded.
    pub async fn fetch_chain_context(&self) -> Result<ChainContext, VmSdkError> {
        let epoch = self
            .client
            .get_epoch(None, None)
            .await
            .map_err(|e| StoreError::new("fetch epoch", e))?
            .into_inner();
        let epoch_id = epoch
            .epoch_id()
            .map_err(|e| StoreError::new("epoch id", e))?;
        let reference_gas_price = epoch
            .gas_price()
            .map_err(|e| StoreError::new("gas price", e))?;
        let epoch_timestamp_ms = epoch
            .start_ms()
            .map_err(|e| StoreError::new("epoch start", e))?;
        let protocol_version = epoch
            .protocol_config()
            .and_then(|pc| pc.version())
            .map_err(|e| StoreError::new("protocol version", e))?;
        Ok(ChainContext {
            protocol_version: iota_protocol_config::ProtocolVersion::new(protocol_version),
            reference_gas_price,
            epoch_id,
            epoch_timestamp_ms,
            chain: iota_protocol_config::Chain::Unknown,
        })
    }

    /// Fetch every object the transaction references and insert it into the
    /// store in one batched request. Owned/immutable objects are fetched at
    /// their transaction versions; shared objects and packages at the latest
    /// version.
    ///
    /// Optional: the store also resolves these objects on demand during
    /// execution. Pre-fetching only saves the per-object round-trips the
    /// executor would otherwise make for the transaction body.
    pub async fn prefetch(&mut self, transaction: &TransactionData) -> Result<(), VmSdkError> {
        let mut refs: Vec<(ObjectId, Option<Version>)> = Vec::new();
        let input_object_kinds = transaction
            .input_objects()
            .map_err(|e| StoreError::new("collect input objects", e))?;
        for kind in &input_object_kinds {
            match kind {
                InputObjectKind::ImmOrOwnedMoveObject(objref) => {
                    refs.push((objref.object_id, Some(objref.version)))
                }
                // Shared objects and packages: latest version.
                InputObjectKind::SharedMoveObject { id, .. } => refs.push((*id, None)),
                InputObjectKind::MovePackage(id) => refs.push((*id, None)),
            }
        }
        for gas_ref in transaction.gas() {
            refs.push((gas_ref.object_id, Some(gas_ref.version)));
        }
        for objref in transaction.receiving_objects() {
            refs.push((objref.object_id, Some(objref.version)));
        }
        if refs.is_empty() {
            return Ok(());
        }
        self.fetch_and_insert(&refs).await
    }

    /// Eagerly fetch the dynamic-field children of every object already cached,
    /// recursively, and insert them too.
    ///
    /// Optional: the store resolves dynamic-field children on demand during
    /// execution, so a run only loads the children it touches (e.g. the slice
    /// of the validator set staking reads). This walks the *entire*
    /// dynamic-field graph instead — useful to pre-warm or snapshot it, but
    /// it over-fetches. Recursion is bounded only by the object graph (a
    /// `visited` set breaks cycles); intended for local development against
    /// trusted nodes.
    ///
    /// # Errors
    ///
    /// Returns [`VmSdkError::Store`] if a listing or fetch fails.
    pub async fn prefetch_dynamic_fields(&mut self) -> Result<(), VmSdkError> {
        let mut visited: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();
        let mut queue: Vec<ObjectId> = self
            .inner
            .lock()
            .expect("store lock poisoned")
            .iter()
            .map(|(id, _)| *id)
            .collect();
        while let Some(parent) = queue.pop() {
            if !visited.insert(parent) {
                continue;
            }
            let fields = self
                .client
                .list_dynamic_fields(parent, None, None, None)
                .collect(None)
                .await
                .map_err(|e| StoreError::new("list dynamic fields", e))?
                .into_inner();
            let mut refs: Vec<(ObjectId, Option<Version>)> = Vec::new();
            for field in fields {
                // The field wrapper object is always needed; for dynamic
                // *object* fields the value lives in a separate child object.
                if let Some(id) = field.field_id {
                    let id = id
                        .object_id()
                        .map_err(|e| StoreError::new("decode dynamic field id", e))?;
                    refs.push((id, None));
                }
                if let Some(id) = field.child_id {
                    let id = id
                        .object_id()
                        .map_err(|e| StoreError::new("decode dynamic child id", e))?;
                    refs.push((id, None));
                }
            }
            if refs.is_empty() {
                continue;
            }
            self.fetch_and_insert(&refs).await?;
            // Recurse into the newly fetched children to find their descendants.
            queue.extend(refs.into_iter().map(|(id, _)| id));
        }
        Ok(())
    }

    /// Fetch and decode objects from the node without caching them.
    async fn fetch_objects(
        &self,
        refs: &[(ObjectId, Option<Version>)],
    ) -> Result<Vec<Object>, VmSdkError> {
        let proto_objects = self
            .client
            .get_objects(refs, None)
            .await
            .map_err(|e| StoreError::new("fetch objects via gRPC", e))?
            .into_inner();
        let mut objects = Vec::with_capacity(proto_objects.len());
        for proto_obj in proto_objects {
            // The proto helper yields the SDK `Object`; round-trip through BCS
            // into the node's `iota_types::object::Object` (identical layout).
            let sdk_obj = proto_obj
                .object()
                .map_err(|e| StoreError::new("decode gRPC object", e))?;
            let bytes =
                bcs::to_bytes(&sdk_obj).map_err(|e| StoreError::new("re-encode object", e))?;
            let obj: Object =
                bcs::from_bytes(&bytes).map_err(|e| StoreError::new("decode object", e))?;
            objects.push(obj);
        }
        Ok(objects)
    }

    /// Fetch `refs` and insert them into the cache.
    async fn fetch_and_insert(
        &self,
        refs: &[(ObjectId, Option<Version>)],
    ) -> Result<(), VmSdkError> {
        let objects = self.fetch_objects(refs).await?;
        let mut inner = self.inner.lock().expect("store lock poisoned");
        for obj in objects {
            inner.insert(obj);
        }
        Ok(())
    }

    /// The most recent on-demand fetch failure, if any.
    ///
    /// The synchronous [`Store`] surface cannot return an error from a cache
    /// miss, so a failed on-demand fetch collapses to "object absent" and later
    /// surfaces as
    /// [`VmSdkError::MissingObject`](crate::VmSdkError::MissingObject).
    /// When a run fails that way, check this to tell a transient transport or
    /// decode failure apart from a genuinely missing object. Shared across
    /// clones; overwritten by each failing fetch.
    pub fn last_fetch_error(&self) -> Option<String> {
        self.last_fetch_error
            .lock()
            .expect("error lock poisoned")
            .clone()
    }

    /// Fetch `refs` synchronously from within the executor by blocking on the
    /// client. A fetch error collapses to an empty result, leaving the object
    /// absent so the VM treats it as missing, but is stashed in
    /// [`last_fetch_error`](Self::last_fetch_error) first. Must run inside a
    /// multi-threaded Tokio runtime.
    fn fetch_blocking(&self, refs: &[(ObjectId, Option<Version>)]) -> Vec<Object> {
        match block_in_place(|| Handle::current().block_on(self.fetch_objects(refs))) {
            Ok(objects) => objects,
            Err(e) => {
                *self.last_fetch_error.lock().expect("error lock poisoned") = Some(e.to_string());
                Vec::new()
            }
        }
    }
}

impl Store for GrpcStore {
    fn get_object(&self, id: &ObjectId, version: Option<Version>) -> Option<Object> {
        // Scope the read lock so it is released before the blocking fetch
        // re-acquires it (a std `Mutex` is not reentrant).
        {
            let inner = self.inner.lock().expect("store lock poisoned");
            if let Some(obj) = inner.get_object(id, version) {
                return Some(obj);
            }
        }
        let fetched = self.fetch_blocking(&[(*id, version)]);
        let mut inner = self.inner.lock().expect("store lock poisoned");
        for obj in fetched {
            inner.insert(obj);
        }
        inner.get_object(id, version)
    }

    fn get_child_object(
        &self,
        parent: &ObjectId,
        child: &ObjectId,
        version_upper_bound: Version,
    ) -> Option<Object> {
        {
            let inner = self.inner.lock().expect("store lock poisoned");
            if let Some(obj) = inner.get_child_object(parent, child, version_upper_bound) {
                return Some(obj);
            }
        }
        // Fetch the child at its latest version; the upper-bound check is
        // re-applied below once it is cached.
        let fetched = self.fetch_blocking(&[(*child, None)]);
        let mut inner = self.inner.lock().expect("store lock poisoned");
        for obj in fetched {
            inner.insert(obj);
        }
        inner.get_child_object(parent, child, version_upper_bound)
    }

    fn insert(&mut self, object: Object) {
        self.inner
            .lock()
            .expect("store lock poisoned")
            .insert(object);
    }

    fn remove(&mut self, id: &ObjectId) {
        self.inner.lock().expect("store lock poisoned").remove(id);
    }
}
