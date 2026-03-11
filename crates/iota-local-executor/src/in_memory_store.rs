// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! A simple in-memory object store for fully offline execution.

use std::collections::BTreeMap;

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

/// A simple in-memory object store backed by a `BTreeMap`.
///
/// All objects must be provided upfront via `insert()`. No network access is
/// performed. This is used by `OfflineExecutor` for fully offline execution.
pub struct InMemoryStore {
    objects: BTreeMap<ObjectID, Object>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
        }
    }

    /// Insert an object into the store.
    pub fn insert(&mut self, object: Object) {
        self.objects.insert(object.id(), object);
    }

    /// Get an object by ID.
    pub fn get_object(&self, id: &ObjectID) -> Option<&Object> {
        self.objects.get(id)
    }

    /// Iterate over all objects in the store.
    pub fn iter(&self) -> impl Iterator<Item = (&ObjectID, &Object)> {
        self.objects.iter()
    }

    /// Return the number of objects in the store.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStore for InMemoryStore {
    fn try_get_object(&self, object_id: &ObjectID) -> Result<Option<Object>, StorageError> {
        Ok(self.objects.get(object_id).cloned())
    }

    fn try_get_object_by_key(
        &self,
        object_id: &ObjectID,
        version: VersionNumber,
    ) -> Result<Option<Object>, StorageError> {
        Ok(self
            .objects
            .get(object_id)
            .filter(|o| o.version() == version)
            .cloned())
    }
}

impl BackingPackageStore for InMemoryStore {
    fn get_package_object(&self, package_id: &ObjectID) -> IotaResult<Option<PackageObject>> {
        Ok(self
            .objects
            .get(package_id)
            .cloned()
            .map(PackageObject::new))
    }
}

impl ChildObjectResolver for InMemoryStore {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        child: &ObjectID,
        child_version_upper_bound: SequenceNumber,
    ) -> IotaResult<Option<Object>> {
        Ok(self
            .objects
            .get(child)
            .filter(|o| o.version() <= child_version_upper_bound)
            .cloned())
    }

    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        receiving_object_id: &ObjectID,
        receive_object_at_version: SequenceNumber,
        _epoch_id: EpochId,
    ) -> IotaResult<Option<Object>> {
        Ok(self
            .objects
            .get(receiving_object_id)
            .filter(|o| o.version() == receive_object_at_version)
            .cloned())
    }
}

// BackingStore is automatically implemented via a blanket impl for any type
// that implements BackingPackageStore + ChildObjectResolver + ObjectStore.
