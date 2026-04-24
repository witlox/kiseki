//! Composition types and operations.

use std::collections::HashMap;
use std::sync::Arc;

use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, ShardId};
use kiseki_log::traits::LogOps;

use crate::error::CompositionError;
use crate::log_bridge;
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
#[async_trait::async_trait]
pub trait CompositionOps: Send + Sync {
    /// Create a new composition in a namespace.
    async fn create(
        &mut self,
        namespace_id: NamespaceId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<CompositionId, CompositionError>;

    /// Read a composition by ID.
    fn get(&self, id: CompositionId) -> Result<&Composition, CompositionError>;

    /// Delete a composition (creates a tombstone delta).
    async fn delete(&mut self, id: CompositionId) -> Result<(), CompositionError>;

    /// Rename a composition. Returns `CrossShardRename` if source and
    /// target are on different shards (I-L8).
    fn rename(
        &mut self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError>;

    /// Update a composition — creates a new version with new chunk refs.
    async fn update(
        &mut self,
        id: CompositionId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<u64, CompositionError>;

    /// Start a multipart upload.
    fn start_multipart(&mut self, namespace_id: NamespaceId) -> Result<String, CompositionError>;

    /// Upload a single part of a multipart upload.
    fn upload_part(
        &mut self,
        upload_id: &str,
        part_number: u32,
        chunk_id: ChunkId,
        size: u64,
    ) -> Result<(), CompositionError>;

    /// Abort a multipart upload — marks parts for GC.
    fn abort_multipart(&mut self, upload_id: &str) -> Result<(), CompositionError>;

    /// Finalize a multipart upload — makes the composition visible (I-L5).
    async fn finalize_multipart(
        &mut self,
        upload_id: &str,
    ) -> Result<CompositionId, CompositionError>;
}

/// In-memory composition store.
///
/// When a `LogOps` implementation is attached via `with_log`, mutations
/// emit deltas to the log shard (Composition → Log data path).
pub struct CompositionStore {
    compositions: HashMap<CompositionId, Composition>,
    namespaces: HashMap<NamespaceId, Namespace>,
    multiparts: HashMap<String, (MultipartUpload, NamespaceId)>,
    log: Option<Arc<dyn LogOps + Send + Sync>>,
}

impl CompositionStore {
    /// Create an empty composition store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compositions: HashMap::new(),
            namespaces: HashMap::new(),
            multiparts: HashMap::new(),
            log: None,
        }
    }

    /// Attach a log store for delta emission. When set, create/update/delete
    /// operations emit deltas to the shard's log.
    #[must_use]
    pub fn with_log(mut self, log: Arc<dyn LogOps + Send + Sync>) -> Self {
        self.log = Some(log);
        self
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

    /// List all compositions in a namespace.
    #[must_use]
    pub fn list_by_namespace(&self, ns_id: NamespaceId) -> Vec<&Composition> {
        self.compositions
            .values()
            .filter(|c| c.namespace_id == ns_id)
            .collect()
    }
}

impl Default for CompositionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl CompositionOps for CompositionStore {
    async fn create(
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
        self.compositions.insert(id, comp.clone());

        // Emit delta to log if attached. Roll back on failure (PIPE-ADV-1).
        if let Some(ref log) = self.log {
            let hashed_key = composition_hash_key(namespace_id, id);
            if !log_bridge::emit_delta(
                log.as_ref(),
                comp.shard_id,
                comp.tenant_id,
                kiseki_log::delta::OperationType::Create,
                hashed_key,
                comp.chunks.clone(),
                id.0.as_bytes().to_vec(),
            )
            .await
            {
                self.compositions.remove(&id);
                return Err(CompositionError::NamespaceNotFound(namespace_id));
            }
        }

        Ok(id)
    }

    fn get(&self, id: CompositionId) -> Result<&Composition, CompositionError> {
        self.compositions
            .get(&id)
            .ok_or(CompositionError::CompositionNotFound(id))
    }

    async fn update(
        &mut self,
        id: CompositionId,
        chunks: Vec<ChunkId>,
        size: u64,
    ) -> Result<u64, CompositionError> {
        let comp = self
            .compositions
            .get_mut(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;
        comp.version += 1;
        comp.chunks.clone_from(&chunks);
        comp.size = size;
        let version = comp.version;
        let shard_id = comp.shard_id;
        let tenant_id = comp.tenant_id;
        let namespace_id = comp.namespace_id;

        if let Some(ref log) = self.log {
            log_bridge::emit_delta(
                log.as_ref(),
                shard_id,
                tenant_id,
                kiseki_log::delta::OperationType::Update,
                composition_hash_key(namespace_id, id),
                chunks,
                id.0.as_bytes().to_vec(),
            )
            .await;
        }

        Ok(version)
    }

    async fn delete(&mut self, id: CompositionId) -> Result<(), CompositionError> {
        let comp = self
            .compositions
            .remove(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;

        if let Some(ref log) = self.log {
            log_bridge::emit_delta(
                log.as_ref(),
                comp.shard_id,
                comp.tenant_id,
                kiseki_log::delta::OperationType::Delete,
                composition_hash_key(comp.namespace_id, id),
                vec![],
                id.0.as_bytes().to_vec(),
            )
            .await;
        }

        Ok(())
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

    fn upload_part(
        &mut self,
        upload_id: &str,
        part_number: u32,
        chunk_id: ChunkId,
        size: u64,
    ) -> Result<(), CompositionError> {
        let (upload, _ns_id) = self
            .multiparts
            .get_mut(upload_id)
            .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

        if !upload.add_part(crate::multipart::MultipartPart {
            part_number,
            chunk_id,
            size,
        }) {
            return Err(CompositionError::MultipartNotFinalized(
                upload_id.to_owned(),
            ));
        }
        Ok(())
    }

    fn abort_multipart(&mut self, upload_id: &str) -> Result<(), CompositionError> {
        let (upload, _ns_id) = self
            .multiparts
            .get_mut(upload_id)
            .ok_or_else(|| CompositionError::MultipartNotFound(upload_id.to_owned()))?;

        if !upload.abort() {
            return Err(CompositionError::MultipartNotFinalized(
                upload_id.to_owned(),
            ));
        }
        Ok(())
    }

    async fn finalize_multipart(
        &mut self,
        upload_id: &str,
    ) -> Result<CompositionId, CompositionError> {
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
        self.create(ns_id, chunks, size).await
    }
}

/// Compute the hashed key for a composition — deterministic routing key.
///
/// Uses UUID v5 (SHA-1 based, deterministic) of `namespace_id` || `composition_id`.
/// Stable across restarts (PIPE-ADV-3).
fn composition_hash_key(ns: NamespaceId, comp: CompositionId) -> [u8; 32] {
    let combined = uuid::Uuid::new_v5(&ns.0, comp.0.as_bytes());
    let mut buf = [0u8; 32];
    buf[..16].copy_from_slice(combined.as_bytes());
    // Mirror to fill 32 bytes deterministically.
    buf[16..32].copy_from_slice(combined.as_bytes());
    buf
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
        let id = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(store.create(test_ns(), vec![ChunkId([0x01; 32])], 1024))
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.chunks.len(), 1);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn delete_removes_composition() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        let id = rt
            .block_on(store.create(test_ns(), vec![], 0))
            .unwrap_or_else(|_| unreachable!());
        rt.block_on(store.delete(id))
            .unwrap_or_else(|_| unreachable!());
        assert!(store.get(id).is_err());
    }

    #[test]
    fn cross_shard_rename_returns_exdev() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        // Add a namespace on a different shard.
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(20)),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(2)), // different shard
            read_only: false,
        });

        let id = rt
            .block_on(store.create(test_ns(), vec![], 0))
            .unwrap_or_else(|_| unreachable!());
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(20)));
        assert!(matches!(
            result,
            Err(CompositionError::CrossShardRename(_, _))
        ));
    }

    #[test]
    fn same_shard_rename_succeeds() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        store.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(11)),
            tenant_id: test_tenant(),
            shard_id: test_shard(), // same shard
            read_only: false,
        });

        let id = rt
            .block_on(store.create(test_ns(), vec![], 0))
            .unwrap_or_else(|_| unreachable!());
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(11)));
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_namespace_rejects_create() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = CompositionStore::new();
        store.add_namespace(Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: true,
        });

        let result = rt.block_on(store.create(test_ns(), vec![], 0));
        assert!(matches!(
            result,
            Err(CompositionError::ReadOnlyNamespace(_))
        ));
    }

    #[test]
    fn multipart_lifecycle() {
        let rt = tokio::runtime::Runtime::new().unwrap();
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

        let comp_id = rt
            .block_on(store.finalize_multipart(&upload_id))
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(comp_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn versioning() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        let id = rt
            .block_on(store.create(test_ns(), vec![ChunkId([0x01; 32])], 100))
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(store.get(id).unwrap_or_else(|_| unreachable!()).version, 1);

        let v2 = rt
            .block_on(store.update(id, vec![ChunkId([0x02; 32]), ChunkId([0x03; 32])], 200))
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.version, 2);
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 200);
    }

    #[test]
    fn composition_belongs_to_one_tenant_ix1() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        let id = rt
            .block_on(store.create(test_ns(), vec![ChunkId([0xaa; 32])], 512))
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        // I-X1: composition is owned by the namespace's tenant.
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.namespace_id, test_ns());
    }

    #[test]
    fn namespace_not_found_returns_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = CompositionStore::new();
        let bogus_ns = NamespaceId(uuid::Uuid::from_u128(999));
        let result = rt.block_on(store.create(bogus_ns, vec![], 0));
        assert!(matches!(
            result,
            Err(CompositionError::NamespaceNotFound(_))
        ));
    }

    #[test]
    fn list_compositions_in_namespace() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();

        let id1 = rt
            .block_on(store.create(test_ns(), vec![ChunkId([0x01; 32])], 100))
            .unwrap_or_else(|_| unreachable!());
        let id2 = rt
            .block_on(store.create(test_ns(), vec![ChunkId([0x02; 32])], 200))
            .unwrap_or_else(|_| unreachable!());
        let id3 = rt
            .block_on(store.create(test_ns(), vec![ChunkId([0x03; 32])], 300))
            .unwrap_or_else(|_| unreachable!());

        let listed = store.list_by_namespace(test_ns());
        assert_eq!(listed.len(), 3);

        let listed_ids: Vec<CompositionId> = listed.iter().map(|c| c.id).collect();
        assert!(listed_ids.contains(&id1));
        assert!(listed_ids.contains(&id2));
        assert!(listed_ids.contains(&id3));
    }

    #[test]
    fn count_tracks_compositions() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut store = setup();
        assert_eq!(store.count(), 0);

        rt.block_on(store.create(test_ns(), vec![], 0))
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 1);

        let id2 = rt
            .block_on(store.create(test_ns(), vec![], 0))
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 2);

        rt.block_on(store.delete(id2))
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 1);
    }
}
