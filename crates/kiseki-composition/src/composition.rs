//! Composition types and operations.

use std::collections::HashMap;

use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, ShardId};

use crate::error::CompositionError;
use crate::multipart::MultipartUpload;
use crate::namespace::Namespace;

/// A composition — metadata describing how to assemble chunks into a
/// coherent data unit (file or object).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Composition {
    /// Composition identifier.
    pub id: CompositionId,
    /// Owning tenant (I-X1).
    pub tenant_id: OrgId,
    /// Parent namespace.
    pub namespace_id: NamespaceId,
    /// Shard this composition's deltas live in.
    pub shard_id: ShardId,
    /// Ordered list of chunk references.
    pub chunks: Vec<ChunkId>,
    /// Current version number.
    pub version: u64,
    /// Total size in bytes.
    pub size: u64,
}

/// Composition operations trait.
pub trait CompositionOps {
    /// Create a new composition in a namespace.
    fn create(
        &mut self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError>;

    /// Read a composition by ID.
    fn get(&self, id: CompositionId) -> Result<&Composition, CompositionError>;

    /// Delete a composition (creates a tombstone delta).
    fn delete(&mut self, id: CompositionId) -> Result<(), CompositionError>;

    /// Rename a composition. Returns `CrossShardRename` if source and
    /// target are on different shards (I-L8).
    fn rename(
        &mut self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError>;

    /// Start a multipart upload.
    fn start_multipart(&mut self, namespace_id: NamespaceId) -> Result<String, CompositionError>;

    /// Finalize a multipart upload — makes the composition visible (I-L5).
    fn finalize_multipart(&mut self, upload_id: &str) -> Result<CompositionId, CompositionError>;
}

/// In-memory composition store.
pub struct CompositionStore {
    compositions: HashMap<CompositionId, Composition>,
    namespaces: HashMap<NamespaceId, Namespace>,
    multiparts: HashMap<String, (MultipartUpload, NamespaceId)>,
}

impl CompositionStore {
    /// Create an empty composition store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compositions: HashMap::new(),
            namespaces: HashMap::new(),
            multiparts: HashMap::new(),
        }
    }

    /// Register a namespace.
    pub fn add_namespace(&mut self, ns: Namespace) {
        self.namespaces.insert(ns.id, ns);
    }

    /// Get a namespace.
    #[must_use]
    pub fn namespace(&self, id: NamespaceId) -> Option<&Namespace> {
        self.namespaces.get(&id)
    }

    /// Total composition count.
    #[must_use]
    pub fn count(&self) -> usize {
        self.compositions.len()
    }
}

impl Default for CompositionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositionOps for CompositionStore {
    fn create(
        &mut self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError> {
        let ns = self
            .namespaces
            .get(&namespace_id)
            .ok_or(CompositionError::NamespaceNotFound(namespace_id))?;

        if ns.read_only {
            return Err(CompositionError::ReadOnlyNamespace(namespace_id));
        }

        let id = CompositionId(uuid::Uuid::new_v4());
        let comp = Composition {
            id,
            tenant_id: ns.tenant_id,
            namespace_id,
            shard_id: ns.shard_id,
            chunks,
            version: 1,
            size,
        };
        self.compositions.insert(id, comp);
        Ok(id)
    }

    fn get(&self, id: CompositionId) -> Result<&Composition, CompositionError> {
        self.compositions
            .get(&id)
            .ok_or(CompositionError::CompositionNotFound(id))
    }

    fn delete(&mut self, id: CompositionId) -> Result<(), CompositionError> {
        self.compositions
            .remove(&id)
            .map(|_| ())
            .ok_or(CompositionError::CompositionNotFound(id))
    }

    fn rename(
        &mut self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError> {
        let comp = self
            .compositions
            .get(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;

        let target_ns = self
            .namespaces
            .get(&target_namespace)
            .ok_or(CompositionError::NamespaceNotFound(target_namespace))?;

        // I-L8: cross-shard rename → EXDEV.
        if comp.shard_id != target_ns.shard_id {
            return Err(CompositionError::CrossShardRename(
                comp.shard_id,
                target_ns.shard_id,
            ));
        }

        let comp = self
            .compositions
            .get_mut(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;
        comp.namespace_id = target_namespace;
        Ok(())
    }

    fn start_multipart(&mut self, namespace_id: NamespaceId) -> Result<String, CompositionError> {
        if !self.namespaces.contains_key(&namespace_id) {
            return Err(CompositionError::NamespaceNotFound(namespace_id));
        }
        let upload_id = uuid::Uuid::new_v4().to_string();
        self.multiparts.insert(
            upload_id.clone(),
            (MultipartUpload::new(upload_id.clone()), namespace_id),
        );
        Ok(upload_id)
    }

    fn finalize_multipart(&mut self, upload_id: &str) -> Result<CompositionId, CompositionError> {
        let (upload, ns_id) = self
            .multiparts
            .get_mut(upload_id)
            .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

        if !upload.finalize() {
            return Err(CompositionError::MultipartNotFinalized(
                upload_id.to_owned(),
            ));
        }

        let chunks: Vec<ChunkId> = upload.parts.iter().map(|p| p.chunk_id).collect();
        let size = upload.total_size();
        let ns_id = *ns_id;

        // Create the composition now that it's visible (I-L5).
        self.create(ns_id, chunks, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    fn test_shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn setup() -> CompositionStore {
        let mut store = CompositionStore::new();
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(10)),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: false,
        });
        store
    }

    fn test_ns() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(10))
    }

    #[test]
    fn create_and_get() {
        let mut store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 1024)
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.chunks.len(), 1);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn delete_removes_composition() {
        let mut store = setup();
        let id = store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        store.delete(id).unwrap_or_else(|_| unreachable!());
        assert!(store.get(id).is_err());
    }

    #[test]
    fn cross_shard_rename_returns_exdev() {
        let mut store = setup();
        // Add a namespace on a different shard.
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(20)),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(2)), // different shard
            read_only: false,
        });

        let id = store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(20)));
        assert!(matches!(
            result,
            Err(CompositionError::CrossShardRename(_, _))
        ));
    }

    #[test]
    fn same_shard_rename_succeeds() {
        let mut store = setup();
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(11)),
            tenant_id: test_tenant(),
            shard_id: test_shard(), // same shard
            read_only: false,
        });

        let id = store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(11)));
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_namespace_rejects_create() {
        let mut store = CompositionStore::new();
        store.add_namespace(Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: true,
        });

        let result = store.create(test_ns(), vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::ReadOnlyNamespace(_))
        ));
    }

    #[test]
    fn multipart_lifecycle() {
        let mut store = setup();
        let upload_id = store
            .start_multipart(test_ns())
            .unwrap_or_else(|_| unreachable!());

        // Add parts directly to the multipart.
        if let Some((upload, _)) = store.multiparts.get_mut(&upload_id) {
            upload.add_part(crate::multipart::MultipartPart {
                part_number: 1,
                chunk_id: ChunkId([0x01; 32]),
                size: 512,
            });
            upload.add_part(crate::multipart::MultipartPart {
                part_number: 2,
                chunk_id: ChunkId([0x02; 32]),
                size: 512,
            });
        }

        let comp_id = store
            .finalize_multipart(&upload_id)
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(comp_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 1024);
    }
}
