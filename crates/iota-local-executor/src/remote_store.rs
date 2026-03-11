// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! A remote-backed object store that fetches objects from a node via JSON-RPC
//! and caches them locally for the duration of a transaction execution.

use std::{collections::BTreeMap, sync::RwLock};

use iota_json_rpc_types::{IotaObjectData, IotaObjectDataOptions};
use iota_sdk::IotaClient;
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

/// An object store backed by a remote IOTA node via JSON-RPC.
///
/// Objects are fetched on-demand and cached in memory. The store is designed to
/// be populated ahead of transaction execution (via `insert`) and then used as
/// a `BackingStore` by the executor. For objects not pre-fetched (e.g., child
/// objects accessed during execution), it will attempt a synchronous fetch
/// via a blocking tokio call.
pub struct RemoteStore {
    client: IotaClient,
    objects: RwLock<BTreeMap<ObjectID, Object>>,
}

impl RemoteStore {
    pub fn new(client: IotaClient) -> Self {
        Self {
            client,
            objects: RwLock::new(BTreeMap::new()),
        }
    }

    /// Insert a pre-fetched object into the local cache.
    pub fn insert(&self, object: Object) {
        self.objects
            .write()
            .expect("lock poisoned")
            .insert(object.id(), object);
    }

    /// Get a cached object by ID.
    pub fn get_object(&self, id: &ObjectID) -> Option<Object> {
        self.objects.read().expect("lock poisoned").get(id).cloned()
    }

    /// Get a cached object by ID and version.
    pub fn get_object_by_key(&self, id: &ObjectID, version: VersionNumber) -> Option<Object> {
        self.objects
            .read()
            .expect("lock poisoned")
            .get(id)
            .filter(|o| o.version() == version)
            .cloned()
    }

    /// Try to fetch an object from the remote node (blocking).
    /// This is used as a fallback during execution when the executor requests
    /// objects that weren't pre-fetched (e.g., dynamically loaded child
    /// objects or packages).
    fn fetch_object_blocking(&self, id: &ObjectID) -> Option<Object> {
        // Check cache first
        if let Some(obj) = self.get_object(id) {
            return Some(obj);
        }

        // Attempt a blocking fetch from the remote node
        let client = self.client.clone();
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
                    match <IotaObjectData as TryInto<Object>>::try_into(data) {
                        Ok(obj) => {
                            self.insert(obj.clone());
                            Some(obj)
                        }
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

impl ObjectStore for RemoteStore {
    fn try_get_object(&self, object_id: &ObjectID) -> Result<Option<Object>, StorageError> {
        Ok(self.fetch_object_blocking(object_id))
    }

    fn try_get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Result<Option<Object>, StorageError> {
        // First check cache with version match
        if let Some(obj) = self.get_object_by_key(object_id, version) {
            return Ok(Some(obj));
        }
        // Fetch from remote (gets latest version)
        let obj = self.fetch_object_blocking(object_id);
        Ok(obj.filter(|o| o.version() == version))
    }
}

impl BackingPackageStore for RemoteStore {
    fn get_package_object(&self, package_id: &ObjectID) -> IotaResult<Option<PackageObject>> {
        let obj = self.fetch_object_blocking(package_id);
        Ok(obj.map(PackageObject::new))
    }
}

impl ChildObjectResolver for RemoteStore {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> IotaResult<Option<Object>> {
        // Fetch the child object; we can only get latest version via RPC, so we check
        // the version bound
        let obj = self.fetch_object_blocking(child);
        Ok(obj.filter(|o| o.version() <= child_version_upper_bound))
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> IotaResult<Option<Object>> {
        let obj = self.fetch_object_blocking(receiving_object_id);
        Ok(obj.filter(|o| o.version() == receive_object_at_version))
    }
}

// BackingStore is automatically implemented via a blanket impl for any type
// that implements BackingPackageStore + ChildObjectResolver + ObjectStore.
