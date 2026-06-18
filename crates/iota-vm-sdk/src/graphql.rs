// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

//! GraphQL-backed store (`feature = "graphql"`, native only).
//!
//! [`GraphqlStore`] mirrors [`crate::grpc::GrpcStore`] but fetches objects over
//! GraphQL. The [`Store`] trait is synchronous and the Move VM resolves objects
//! on demand mid-execution, so a cache miss is served by blocking on the client
//! to fetch that object, then caching it. Only the objects a run actually
//! touches are fetched; [`prefetch`](GraphqlStore::prefetch) is an optional
//! warm-up.
//!
//! On-demand fetching blocks the executor thread on async I/O via
//! [`block_in_place`], so [`LocalVm::execute`](crate::LocalVm::execute) must
//! run inside a multi-threaded Tokio runtime (e.g. `#[tokio::main]`).

use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iota_graphql_rpc_client::simple_client::SimpleClient;
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

/// A [`Store`] backed by a remote node over GraphQL, resolving objects on
/// demand.
///
/// Clones share the same cache and client.
#[derive(Clone)]
pub struct GraphqlStore {
    inner: Arc<Mutex<InMemoryStore>>,
    client: SimpleClient,
    last_fetch_error: Arc<Mutex<Option<String>>>,
}

impl GraphqlStore {
    /// Wrap an existing client. The store starts with the built-in framework
    /// packages already loaded so Move calls resolve.
    pub fn new(client: SimpleClient) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InMemoryStore::with_framework())),
            client,
            last_fetch_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Connect to a GraphQL endpoint (by URL) and create a store containing
    /// only the built-in framework packages.
    ///
    /// Returns a `Result` to mirror
    /// [`GrpcStore::connect`](crate::grpc::GrpcStore::connect); building the
    /// client is currently infallible.
    pub fn connect(url: impl Into<String>) -> Result<Self, VmSdkError> {
        Ok(Self::new(SimpleClient::new(url)))
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
    /// Returns [`VmSdkError::Store`] if the query fails or epoch fields are
    /// missing.
    pub async fn fetch_chain_context(&self) -> Result<ChainContext, VmSdkError> {
        let query = r#"{
            epoch {
                epochId
                referenceGasPrice
                startTimestamp
                protocolConfigs { protocolVersion }
            }
        }"#;
        let json = self
            .client
            .execute(query.to_string(), vec![])
            .await
            .map_err(|e| StoreError::new("fetch epoch via GraphQL", e))?;
        let epoch = json
            .pointer("/data/epoch")
            .ok_or_else(|| StoreError::new("GraphQL epoch", "missing epoch data"))?;
        let epoch_id = epoch
            .get("epochId")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| StoreError::new("GraphQL epoch", "missing epochId"))?;
        let reference_gas_price = epoch
            .get("referenceGasPrice")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| StoreError::new("GraphQL epoch", "missing referenceGasPrice"))?;
        let start_timestamp = epoch
            .get("startTimestamp")
            .and_then(|v| v.as_str())
            .ok_or_else(|| StoreError::new("GraphQL epoch", "missing startTimestamp"))?;
        let epoch_timestamp_ms =
            parse_start_timestamp_millis(start_timestamp).ok_or_else(|| {
                StoreError::new(
                    "GraphQL epoch",
                    format!("invalid startTimestamp {start_timestamp:?}"),
                )
            })?;
        let protocol_version = epoch
            .pointer("/protocolConfigs/protocolVersion")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| StoreError::new("GraphQL epoch", "missing protocolVersion"))?;
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
    /// recursively, and insert them too. Mirrors
    /// [`GrpcStore::prefetch_dynamic_fields`](crate::grpc::GrpcStore::prefetch_dynamic_fields).
    ///
    /// Optional: the store resolves dynamic-field children on demand during
    /// execution, so a run only loads the children it touches (e.g. the slice
    /// of the validator set staking reads). This walks the *entire*
    /// dynamic-field graph instead — useful to pre-warm or snapshot it, but
    /// it over-fetches. Recursion is bounded only by the object graph (a
    /// `visited` set breaks cycles); intended for local development against
    /// trusted nodes.
    ///
    /// The GraphQL `dynamicFields` connection returns each field's name and
    /// value but not the `Field` wrapper object's id, so the wrapper id is
    /// derived from the field name (its type and BCS bytes) the same way the
    /// Move VM derives it on-chain.
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
            let ids = self.list_dynamic_field_object_ids(parent).await?;
            if ids.is_empty() {
                continue;
            }
            let refs: Vec<(ObjectId, Option<Version>)> = ids.iter().map(|id| (*id, None)).collect();
            self.fetch_and_insert(&refs).await?;
            // Recurse into the newly fetched children to find their descendants.
            queue.extend(ids);
        }
        Ok(())
    }

    /// List the object ids that make up `parent`'s dynamic fields: the derived
    /// `Field` wrapper for every field, plus the separate child object of each
    /// dynamic *object* field.
    async fn list_dynamic_field_object_ids(
        &self,
        parent: ObjectId,
    ) -> Result<Vec<ObjectId>, VmSdkError> {
        let mut ids: Vec<ObjectId> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let after = match &cursor {
                Some(c) => format!(r#", after: "{c}""#),
                None => String::new(),
            };
            let query = format!(
                r#"{{ object(address: "{parent}") {{ dynamicFields(first: 50{after}) {{
                    pageInfo {{ hasNextPage endCursor }}
                    nodes {{
                        name {{ type {{ repr }} bcs }}
                        value {{ __typename ... on MoveObject {{ address }} }}
                    }}
                }} }} }}"#
            );
            let json = self
                .client
                .execute(query, vec![])
                .await
                .map_err(|e| StoreError::new("list dynamic fields via GraphQL", e))?;
            let Some(fields) = json.pointer("/data/object/dynamicFields") else {
                break;
            };
            if let Some(nodes) = fields.get("nodes").and_then(|v| v.as_array()) {
                for node in nodes {
                    if let Some(field_id) = derive_field_wrapper_id(parent, node) {
                        ids.push(field_id);
                    }
                    // A dynamic *object* field keeps its value in a separate
                    // child object, addressable directly.
                    if node.pointer("/value/__typename").and_then(|v| v.as_str())
                        == Some("MoveObject")
                    {
                        if let Some(id) = node
                            .pointer("/value/address")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<ObjectId>().ok())
                        {
                            ids.push(id);
                        }
                    }
                }
            }
            let has_next = fields
                .pointer("/pageInfo/hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            cursor = fields
                .pointer("/pageInfo/endCursor")
                .and_then(|v| v.as_str())
                .map(String::from);
            if !has_next || cursor.is_none() {
                break;
            }
        }
        Ok(ids)
    }

    /// Fetch and decode objects from the node without caching them. Each ref is
    /// fetched at its given version, or the latest version when `None`.
    async fn fetch_objects(
        &self,
        refs: &[(ObjectId, Option<Version>)],
    ) -> Result<Vec<Object>, VmSdkError> {
        let mut aliases: Vec<String> = Vec::with_capacity(refs.len());
        for (id, version) in refs {
            match version {
                Some(v) => aliases.push(format!(
                    r#"v{}: object(address: "{id}", version: {}) {{ bcs }}"#,
                    aliases.len(),
                    v.as_u64()
                )),
                None => aliases.push(format!(
                    r#"v{}: object(address: "{id}") {{ bcs }}"#,
                    aliases.len()
                )),
            }
        }
        let query = format!("{{ {} }}", aliases.join("\n"));
        let json = self
            .client
            .execute(query, vec![])
            .await
            .map_err(|e| StoreError::new("GraphQL query", e))?;
        let data = json
            .get("data")
            .ok_or_else(|| StoreError::new("GraphQL response", "missing data"))?;
        let mut objects = Vec::new();
        if let Some(obj_map) = data.as_object() {
            for (alias, value) in obj_map {
                if let Some(bcs_b64) = value.pointer("/bcs").and_then(|v| v.as_str()) {
                    let bytes = BASE64
                        .decode(bcs_b64)
                        .map_err(|e| StoreError::new(format!("decode {alias}"), e))?;
                    let obj: Object = bcs::from_bytes(&bytes)
                        .map_err(|e| StoreError::new(format!("bcs {alias}"), e))?;
                    objects.push(obj);
                }
            }
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

/// Derive the on-chain `Field` wrapper object id for a `dynamicFields` node
/// from its name (the field's type repr and BCS bytes), matching the Move VM's
/// derivation. Returns `None` if the node is missing a name or it can't be
/// parsed.
fn derive_field_wrapper_id(parent: ObjectId, node: &serde_json::Value) -> Option<ObjectId> {
    let repr = node.pointer("/name/type/repr").and_then(|v| v.as_str())?;
    let name_bcs = node.pointer("/name/bcs").and_then(|v| v.as_str())?;
    let tag = repr.parse::<iota_sdk_types::TypeTag>().ok()?;
    let name_bytes = BASE64.decode(name_bcs).ok()?;
    iota_types::dynamic_field::derive_dynamic_field_id(*parent.as_address(), &tag, &name_bytes).ok()
}

/// Parse the GraphQL `startTimestamp` scalar — an RFC 3339 datetime string
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`), or plain integer milliseconds — to
/// milliseconds since the Unix epoch.
fn parse_start_timestamp_millis(s: &str) -> Option<u64> {
    if let Ok(ms) = s.parse::<u64>() {
        return Some(ms);
    }
    let datetime = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    u64::try_from(datetime.timestamp_millis()).ok()
}

impl Store for GraphqlStore {
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

#[cfg(test)]
mod tests {
    use super::parse_start_timestamp_millis;

    #[test]
    fn start_timestamp_parses_rfc3339_and_integer_millis() {
        assert_eq!(
            parse_start_timestamp_millis("2023-08-19T15:37:24.761Z"),
            Some(1_692_459_444_761)
        );
        assert_eq!(
            parse_start_timestamp_millis("2023-08-19T15:37:24Z"),
            Some(1_692_459_444_000)
        );
        assert_eq!(
            parse_start_timestamp_millis("1692459444761"),
            Some(1_692_459_444_761)
        );
        assert_eq!(parse_start_timestamp_millis("not a date"), None);
    }
}
