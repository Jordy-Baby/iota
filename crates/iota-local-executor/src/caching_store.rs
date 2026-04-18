// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! A generic caching object store parameterised by a fetcher strategy.
//!
//! [`CachingStore<F>`] holds an in-memory cache and delegates on-demand fetches
//! to an [`ObjectFetcher`] implementation. Three built-in fetchers cover the
//! supported transport backends:
//!
//! - [`JsonRpcFetcher`] — fetches via JSON-RPC (`IotaClient`)
//! - [`GrpcFetcher`] — fetches via gRPC (`iota_grpc_client::Client`)
//! - [`GraphqlFetcher`] — fetches via GraphQL (`SimpleClient`)

use std::{collections::BTreeMap, sync::RwLock};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iota_graphql_rpc_client::simple_client::SimpleClient;
use iota_grpc_client::Client as GrpcClient;
use iota_json_rpc_types::IotaObjectDataOptions;
use iota_sdk::IotaClient;
use iota_sdk_types::ObjectId;
use iota_types::{
    base_types::{ObjectID, SequenceNumber, VersionNumber},
    committee::EpochId,
    error::IotaResult,
    object::Object,
    storage::{
        BackingPackageStore, ChildObjectResolver, ObjectStore, PackageObject,
        error::Error as StorageError,
    },
};

// ---------------------------------------------------------------------------
// ObjectFetcher trait
// ---------------------------------------------------------------------------

/// Strategy for fetching a single object from a remote node.
///
/// Implementations are called from synchronous `ObjectStore` / `BackingStore`
/// trait methods, so they must use blocking I/O internally (e.g.,
/// `tokio::task::block_in_place`).
pub trait ObjectFetcher: Send + Sync {
    fn fetch_object(&self, id: &ObjectID) -> Option<Object>;
}

// ---------------------------------------------------------------------------
// CachingStore<F>
// ---------------------------------------------------------------------------

/// An object store that caches fetched objects in memory and falls back to an
/// [`ObjectFetcher`] for cache misses.
pub struct CachingStore<F> {
    fetcher: F,
    objects: RwLock<BTreeMap<ObjectID, Object>>,
    /// Package overrides checked before any remote fetch. Used by
    /// `LocalPackage` to expose locally-compiled Move packages under a
    /// synthetic [`ObjectID`] without publishing them on-chain.
    package_overrides: RwLock<BTreeMap<ObjectID, Object>>,
}

impl<F> CachingStore<F> {
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            objects: RwLock::new(BTreeMap::new()),
            package_overrides: RwLock::new(BTreeMap::new()),
        }
    }

    /// Insert a pre-fetched object into the local cache.
    pub fn insert(&self, object: Object) {
        self.objects
            .write()
            .expect("lock poisoned")
            .insert(object.id(), object);
    }

    /// Install a local package under a synthetic [`ObjectID`]. Future
    /// `BackingPackageStore::get_package_object` and `ObjectStore` reads for
    /// this ID return the override instead of hitting the remote fetcher.
    ///
    /// The supplied `package_object` must wrap a `MovePackage` (not a Move
    /// object) — this is what `LocalPackage::install_into_caching` produces.
    pub fn insert_package_override(&self, package_object: Object) {
        self.package_overrides
            .write()
            .expect("lock poisoned")
            .insert(package_object.id(), package_object);
    }

    /// Get a cached object by ID (cache-only, no fetch).
    pub fn get_cached(&self, id: &ObjectID) -> Option<Object> {
        self.objects.read().expect("lock poisoned").get(id).cloned()
    }

    /// Look up a package override without touching the object cache or remote
    /// fetcher.
    fn get_package_override(&self, id: &ObjectID) -> Option<Object> {
        self.package_overrides
            .read()
            .expect("lock poisoned")
            .get(id)
            .cloned()
    }
}

impl<F: ObjectFetcher> CachingStore<F> {
    /// Get an object by ID, fetching from the remote if not cached. Package
    /// overrides shadow both the cache and the remote fetch.
    fn get_or_fetch(&self, id: &ObjectID) -> Option<Object> {
        if let Some(obj) = self.get_package_override(id) {
            return Some(obj);
        }
        if let Some(obj) = self.get_cached(id) {
            return Some(obj);
        }
        let obj = self.fetcher.fetch_object(id)?;
        self.insert(obj.clone());
        Some(obj)
    }
}

impl<F: ObjectFetcher> ObjectStore for CachingStore<F> {
    fn try_get_object(&self, object_id: &ObjectID) -> Result<Option<Object>, StorageError> {
        Ok(self.get_or_fetch(object_id))
    }

    fn try_get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Result<Option<Object>, StorageError> {
        // Check cache with version match first.
        if let Some(obj) = self
            .objects
            .read()
            .expect("lock poisoned")
            .get(object_id)
            .filter(|o| o.version() == version)
            .cloned()
        {
            return Ok(Some(obj));
        }
        let obj = self.get_or_fetch(object_id);
        Ok(obj.filter(|o| o.version() == version))
    }
}

impl<F: ObjectFetcher> BackingPackageStore for CachingStore<F> {
    fn get_package_object(&self, package_id: &ObjectID) -> IotaResult<Option<PackageObject>> {
        Ok(self.get_or_fetch(package_id).map(PackageObject::new))
    }
}

impl<F: ObjectFetcher> ChildObjectResolver for CachingStore<F> {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> IotaResult<Option<Object>> {
        let obj = self.get_or_fetch(child);
        Ok(obj.filter(|o| o.version() <= child_version_upper_bound))
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> IotaResult<Option<Object>> {
        let obj = self.get_or_fetch(receiving_object_id);
        Ok(obj.filter(|o| o.version() == receive_object_at_version))
    }
}

// ---------------------------------------------------------------------------
// JsonRpcFetcher
// ---------------------------------------------------------------------------

/// Fetches objects from a remote IOTA node via JSON-RPC.
pub struct JsonRpcFetcher(pub(crate) IotaClient);

impl ObjectFetcher for JsonRpcFetcher {
    fn fetch_object(&self, id: &ObjectID) -> Option<Object> {
        let client = self.0.clone();
        let id = *id;
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let options = IotaObjectDataOptions::full_content()
                    .with_bcs()
                    .with_owner()
                    .with_previous_transaction();
                client.read_api().get_object_with_options(id, options).await
            })
        });

        match result {
            Ok(response) => {
                if let Some(data) = response.data {
                    match <iota_json_rpc_types::IotaObjectData as TryInto<Object>>::try_into(data) {
                        Ok(obj) => Some(obj),
                        Err(e) => {
                            tracing::warn!("Failed to convert object {id}: {e}");
                            None
                        }
                    }
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::warn!("Failed to fetch object {id} from remote: {e}");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GrpcFetcher
// ---------------------------------------------------------------------------

/// Fetches objects from a remote IOTA node via gRPC.
pub struct GrpcFetcher(pub(crate) GrpcClient);

impl ObjectFetcher for GrpcFetcher {
    fn fetch_object(&self, id: &ObjectID) -> Option<Object> {
        let client = self.0.clone();
        let sdk_id: ObjectId = (*id).into();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { client.get_objects(&[(sdk_id, None)], None).await })
        });

        match result {
            Ok(envelope) => {
                for proto_obj in envelope.into_inner() {
                    match proto_obj.object() {
                        Ok(sdk_obj) => match Object::try_from(sdk_obj) {
                            Ok(obj) => return Some(obj),
                            Err(e) => {
                                tracing::warn!("Failed to convert object {id}: {e}");
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Failed to deserialize object {id}: {e}");
                        }
                    }
                }
                None
            }
            Err(e) => {
                tracing::warn!("Failed to fetch object {id} via gRPC: {e}");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GraphqlFetcher
// ---------------------------------------------------------------------------

/// Fetches objects from a remote IOTA node via GraphQL.
pub struct GraphqlFetcher(pub(crate) SimpleClient);

impl ObjectFetcher for GraphqlFetcher {
    fn fetch_object(&self, id: &ObjectID) -> Option<Object> {
        let client = self.0.clone();
        let query = format!(r#"{{ object(address: "{id}") {{ bcs }} }}"#);

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async { client.execute(query, vec![]).await })
        });

        match result {
            Ok(json) => {
                let bcs_b64 = json.pointer("/data/object/bcs").and_then(|v| v.as_str())?;
                match BASE64.decode(bcs_b64) {
                    Ok(bytes) => match bcs::from_bytes::<Object>(&bytes) {
                        Ok(obj) => Some(obj),
                        Err(e) => {
                            tracing::warn!("Failed to deserialize BCS for object {id}: {e}");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!("Failed to decode Base64 for object {id}: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to fetch object {id} via GraphQL: {e}");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Type aliases (preserve public API)
// ---------------------------------------------------------------------------

/// An object store backed by JSON-RPC.
pub type JsonRpcStore = CachingStore<JsonRpcFetcher>;
/// An object store backed by gRPC.
pub type GrpcStore = CachingStore<GrpcFetcher>;
/// An object store backed by GraphQL.
pub type GraphqlStore = CachingStore<GraphqlFetcher>;
