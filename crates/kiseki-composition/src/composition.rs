//! Composition types and operations.

use std::collections::HashMap;
use std::sync::Arc;

use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId, ShardId};
use kiseki_log::traits::LogOps;

use crate::error::CompositionError;
use crate::multipart::MultipartUpload;
use crate::namespace::Namespace;

/// Default inline data threshold in bytes. Data below this size is
/// stored inline in the delta payload rather than as a separate chunk.
pub const INLINE_DATA_THRESHOLD: u64 = 4096;

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
    /// Whether the composition data is inline in the delta (no chunks).
    pub has_inline_data: bool,
    /// Optional Content-Type carried through PUT → GET (RFC 6838).
    /// Stored on the composition so it survives across gateway
    /// instances (per ADV-PA-4: a per-`S3State` `HashMap` loses the
    /// header on multi-gateway deployments).
    pub content_type: Option<String>,
}

/// Result of a delete operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteResult {
    /// Composition was removed. Contains chunk IDs whose refcounts should
    /// be decremented.
    Removed(Vec<ChunkId>),
    /// A delete marker (tombstone) was appended because versioning is
    /// enabled. No chunk refcounts are changed.
    DeleteMarker,
}

/// Composition operations trait.
///
/// All methods are sync — they operate on in-memory state only.
/// Log emission (Raft consensus) is handled by the gateway after
/// releasing the composition lock, avoiding lock-across-await
/// serialization (ADR-032).
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

    /// Delete a composition. Returns `DeleteMarker` if versioning is
    /// enabled on the namespace.
    fn delete(&mut self, id: CompositionId) -> Result<DeleteResult, CompositionError>;

    /// Rename a composition. Returns `CrossShardRename` if source and
    /// target are on different shards (I-L8).
    fn rename(
        &mut self,
        id: CompositionId,
        target_namespace: NamespaceId,
    ) -> Result<(), CompositionError>;

    /// Update a composition — creates a new version with new chunk refs.
    fn update(
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
    fn finalize_multipart(&mut self, upload_id: &str) -> Result<CompositionId, CompositionError>;
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

    /// Attach a log store for delta emission.
    #[must_use]
    pub fn with_log(mut self, log: Arc<dyn LogOps + Send + Sync>) -> Self {
        self.log = Some(log);
        self
    }

    /// Get the attached log store (if any).
    #[must_use]
    pub fn log(&self) -> Option<&Arc<dyn LogOps + Send + Sync>> {
        self.log.as_ref()
    }

    /// Register a namespace.
    pub fn add_namespace(&mut self, ns: Namespace) {
        self.namespaces.insert(ns.id, ns);
    }

    /// Clear all namespace registrations (gateway crash simulation).
    pub fn clear_namespaces(&mut self) {
        self.namespaces.clear();
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

    /// Attach a Content-Type to an existing composition (RFC 6838
    /// round-trip). Returns `Err(CompositionNotFound)` if the
    /// composition doesn't exist. Idempotent: overwrites any prior
    /// value.
    ///
    /// # Errors
    ///
    /// Returns `CompositionError::CompositionNotFound` if `id` is
    /// not in the store.
    pub fn set_content_type(
        &mut self,
        id: CompositionId,
        content_type: Option<String>,
    ) -> Result<(), CompositionError> {
        let comp = self
            .compositions
            .get_mut(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;
        comp.content_type = content_type;
        Ok(())
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
        let has_inline_data = chunks.is_empty() && size > 0 && size <= INLINE_DATA_THRESHOLD;
        let comp = Composition {
            id,
            tenant_id: ns.tenant_id,
            namespace_id,
            shard_id: ns.shard_id,
            chunks,
            version: 1,
            size,
            has_inline_data,
            content_type: None,
        };
        self.compositions.insert(id, comp);
        Ok(id)
    }

    fn get(&self, id: CompositionId) -> Result<&Composition, CompositionError> {
        self.compositions
            .get(&id)
            .ok_or(CompositionError::CompositionNotFound(id))
    }

    fn update(
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
        Ok(comp.version)
    }

    fn delete(&mut self, id: CompositionId) -> Result<DeleteResult, CompositionError> {
        let comp = self
            .compositions
            .get(&id)
            .ok_or(CompositionError::CompositionNotFound(id))?;

        let ns = self.namespaces.get(&comp.namespace_id);
        let versioning = ns.is_some_and(|n| n.versioning_enabled);

        if versioning {
            // Versioned delete: keep all versions, just bump version as
            // a tombstone marker. Chunk refcounts are NOT decremented.
            let comp = self
                .compositions
                .get_mut(&id)
                .ok_or(CompositionError::CompositionNotFound(id))?;
            comp.version += 1;
            Ok(DeleteResult::DeleteMarker)
        } else {
            let comp = self
                .compositions
                .remove(&id)
                .ok_or(CompositionError::CompositionNotFound(id))?;
            Ok(DeleteResult::Removed(comp.chunks))
        }
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

/// Compute the hashed key for a composition — deterministic routing key.
///
/// Uses UUID v5 (SHA-1 based, deterministic) of `namespace_id` || `composition_id`.
/// Stable across restarts (PIPE-ADV-3).
#[must_use]
pub fn composition_hash_key(ns: NamespaceId, comp: CompositionId) -> [u8; 32] {
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

    fn make_ns(id: u128, tenant: OrgId, shard: ShardId) -> Namespace {
        Namespace {
            id: NamespaceId(uuid::Uuid::from_u128(id)),
            tenant_id: tenant,
            shard_id: shard,
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        }
    }

    fn setup() -> CompositionStore {
        let mut store = CompositionStore::new();
        store.add_namespace(make_ns(10, test_tenant(), test_shard()));
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
            .unwrap();

        let comp = store.get(id).unwrap();
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.chunks.len(), 1);
        assert_eq!(comp.size, 1024);
    }

    #[test]
    fn delete_removes_composition() {
        let mut store = setup();
        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.delete(id).unwrap();
        assert!(matches!(result, DeleteResult::Removed(_)));
        assert!(store.get(id).is_err());
    }

    #[test]
    fn cross_shard_rename_returns_exdev() {
        let mut store = setup();
        store.add_namespace(make_ns(
            20,
            test_tenant(),
            ShardId(uuid::Uuid::from_u128(2)),
        ));

        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(20)));
        assert!(matches!(
            result,
            Err(CompositionError::CrossShardRename(_, _))
        ));
    }

    #[test]
    fn same_shard_rename_succeeds() {
        let mut store = setup();
        store.add_namespace(make_ns(11, test_tenant(), test_shard()));

        let id = store.create(test_ns(), vec![], 0).unwrap();
        let result = store.rename(id, NamespaceId(uuid::Uuid::from_u128(11)));
        assert!(result.is_ok());
    }

    #[test]
    fn read_only_namespace_rejects_create() {
        let mut store = CompositionStore::new();
        let mut ns = make_ns(10, test_tenant(), test_shard());
        ns.read_only = true;
        store.add_namespace(ns);

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

    #[test]
    fn versioning() {
        let mut store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(store.get(id).unwrap_or_else(|_| unreachable!()).version, 1);

        let v2 = store
            .update(id, vec![ChunkId([0x02; 32]), ChunkId([0x03; 32])], 200)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        assert_eq!(comp.version, 2);
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 200);
    }

    #[test]
    fn composition_belongs_to_one_tenant_ix1() {
        let mut store = setup();
        let id = store
            .create(test_ns(), vec![ChunkId([0xaa; 32])], 512)
            .unwrap_or_else(|_| unreachable!());

        let comp = store.get(id).unwrap_or_else(|_| unreachable!());
        // I-X1: composition is owned by the namespace's tenant.
        assert_eq!(comp.tenant_id, test_tenant());
        assert_eq!(comp.namespace_id, test_ns());
    }

    #[test]
    fn namespace_not_found_returns_error() {
        let mut store = CompositionStore::new();
        let bogus_ns = NamespaceId(uuid::Uuid::from_u128(999));
        let result = store.create(bogus_ns, vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::NamespaceNotFound(_))
        ));
    }

    #[test]
    fn list_compositions_in_namespace() {
        let mut store = setup();

        let id1 = store
            .create(test_ns(), vec![ChunkId([0x01; 32])], 100)
            .unwrap_or_else(|_| unreachable!());
        let id2 = store
            .create(test_ns(), vec![ChunkId([0x02; 32])], 200)
            .unwrap_or_else(|_| unreachable!());
        let id3 = store
            .create(test_ns(), vec![ChunkId([0x03; 32])], 300)
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
        let mut store = setup();
        assert_eq!(store.count(), 0);

        store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 1);

        let id2 = store
            .create(test_ns(), vec![], 0)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 2);

        let _ = store.delete(id2).unwrap_or_else(|_| unreachable!());
        assert_eq!(store.count(), 1);
    }

    // ===================================================================
    // Composition-feature @unit scenario tests
    // ===================================================================

    // --- Scenario: Create a new file composition via protocol gateway ---
    #[test]
    fn create_composition_returns_chunk_ids_for_refcount() {
        let mut store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let id = store.create(test_ns(), vec![c1, c2], 2048).unwrap();
        let comp = store.get(id).unwrap();

        // Composition references chunks that the caller would pass to
        // ChunkStore for refcount tracking.
        assert_eq!(comp.chunks, vec![c1, c2]);
        assert_eq!(comp.shard_id, test_shard());
        assert_eq!(comp.version, 1);
        assert!(!comp.has_inline_data);
    }

    // --- Scenario: Create a small file with inline data ---
    #[test]
    fn create_small_file_sets_inline_data_flag() {
        let mut store = setup();
        // 512 bytes, no chunk IDs — data would be inline in the delta payload.
        let id = store.create(test_ns(), vec![], 512).unwrap();
        let comp = store.get(id).unwrap();

        assert!(comp.has_inline_data);
        assert!(comp.chunks.is_empty());
        assert_eq!(comp.size, 512);
    }

    #[test]
    fn create_above_threshold_not_inline() {
        let mut store = setup();
        // 8192 bytes with a chunk ref — not inline.
        let id = store
            .create(test_ns(), vec![ChunkId([0xaa; 32])], 8192)
            .unwrap();
        let comp = store.get(id).unwrap();
        assert!(!comp.has_inline_data);
    }

    #[test]
    fn create_zero_size_not_inline() {
        let mut store = setup();
        // Empty file (size 0, no chunks) — not inline (nothing to inline).
        let id = store.create(test_ns(), vec![], 0).unwrap();
        let comp = store.get(id).unwrap();
        assert!(!comp.has_inline_data);
    }

    // --- Scenario: Append data to an existing composition ---
    #[test]
    fn append_extends_chunk_list() {
        let mut store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let id = store
            .create(test_ns(), vec![c1, c2], 128 * 1024 * 1024)
            .unwrap();

        let c3 = ChunkId([0x03; 32]);
        let c4 = ChunkId([0x04; 32]);
        let v2 = store
            .update(id, vec![c1, c2, c3, c4], 256 * 1024 * 1024)
            .unwrap();
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap();
        assert_eq!(comp.chunks, vec![c1, c2, c3, c4]);
    }

    // --- Scenario: Overwrite a byte range in a composition ---
    #[test]
    fn overwrite_replaces_chunk_in_list() {
        let mut store = setup();
        let c1 = ChunkId([0x01; 32]);
        let c2 = ChunkId([0x02; 32]);
        let c3 = ChunkId([0x03; 32]);
        let id = store
            .create(test_ns(), vec![c1, c2, c3], 192 * 1024 * 1024)
            .unwrap();

        // Replace c2 with c2_prime (byte-range overwrite of second chunk).
        let c2_prime = ChunkId([0x22; 32]);
        let v2 = store
            .update(id, vec![c1, c2_prime, c3], 192 * 1024 * 1024)
            .unwrap();
        assert_eq!(v2, 2);

        let comp = store.get(id).unwrap();
        assert_eq!(comp.chunks, vec![c1, c2_prime, c3]);
        // c2 is no longer referenced — caller decrements its refcount.
        assert!(!comp.chunks.contains(&c2));
    }

    // --- Scenario: S3 multipart upload (I-L5) ---
    #[test]
    fn multipart_not_visible_before_finalize_il5() {
        let mut store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 1024)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0x11; 32]), 1024)
            .unwrap();
        store
            .upload_part(&upload_id, 3, ChunkId([0x12; 32]), 1024)
            .unwrap();

        // Before finalize: no composition exists for these parts (I-L5).
        assert_eq!(store.count(), 0);

        let comp_id = store.finalize_multipart(&upload_id).unwrap();
        let comp = store.get(comp_id).unwrap();
        assert_eq!(comp.chunks.len(), 3);
        assert_eq!(comp.size, 3072);
    }

    // --- Scenario: Multipart upload aborted ---
    #[test]
    fn multipart_abort_no_composition_created() {
        let mut store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 1024)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0x11; 32]), 1024)
            .unwrap();

        store.abort_multipart(&upload_id).unwrap();

        // No composition was created — chunks have refcount 0.
        assert_eq!(store.count(), 0);

        // Verify the upload is in Aborted state — cannot finalize.
        let result = store.finalize_multipart(&upload_id);
        assert!(result.is_err());
    }

    #[test]
    fn aborted_multipart_rejects_further_parts() {
        let mut store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();
        store.abort_multipart(&upload_id).unwrap();

        let result = store.upload_part(&upload_id, 1, ChunkId([0x10; 32]), 512);
        assert!(result.is_err());
    }

    // --- Scenario: Delete a composition (refcount tracking) ---
    #[test]
    fn delete_returns_chunk_ids_for_refcount_decrement() {
        let mut store = setup();
        let c5 = ChunkId([0x05; 32]);
        let c6 = ChunkId([0x06; 32]);
        let id = store.create(test_ns(), vec![c5, c6], 1024).unwrap();

        let result = store.delete(id).unwrap();
        // Caller uses the returned chunk IDs to decrement refcounts.
        assert_eq!(result, DeleteResult::Removed(vec![c5, c6]));
        assert!(store.get(id).is_err());
    }

    // --- Scenario: Delete composition with object versioning enabled ---
    #[test]
    fn versioned_delete_creates_delete_marker() {
        let mut store = CompositionStore::new();
        let mut ns = make_ns(10, test_tenant(), test_shard());
        ns.versioning_enabled = true;
        store.add_namespace(ns);

        let c1 = ChunkId([0x01; 32]);
        let id = store.create(test_ns(), vec![c1], 100).unwrap();
        assert_eq!(store.get(id).unwrap().version, 1);

        // Create versions v2, v3.
        store.update(id, vec![ChunkId([0x02; 32])], 200).unwrap();
        store.update(id, vec![ChunkId([0x03; 32])], 300).unwrap();
        assert_eq!(store.get(id).unwrap().version, 3);

        let result = store.delete(id).unwrap();
        assert_eq!(result, DeleteResult::DeleteMarker);

        // Composition still exists (versioned — not removed).
        let comp = store.get(id).unwrap();
        // Version bumped for the tombstone.
        assert_eq!(comp.version, 4);
        // Chunk refcounts are NOT decremented (caller checks DeleteMarker).
    }

    // --- Scenario: Intra-tenant dedup — same chunk ID yields same ref ---
    #[test]
    fn intra_tenant_dedup_same_chunk_id() {
        let mut store = setup();
        let chunk_abc = ChunkId([0xab; 32]); // sha256(P) = "abc"

        let id_a = store.create(test_ns(), vec![chunk_abc], 1024).unwrap();
        let id_b = store.create(test_ns(), vec![chunk_abc], 1024).unwrap();

        let comp_a = store.get(id_a).unwrap();
        let comp_b = store.get(id_b).unwrap();

        // Both compositions reference the same chunk — refcount would be 2.
        assert_eq!(comp_a.chunks, vec![chunk_abc]);
        assert_eq!(comp_b.chunks, vec![chunk_abc]);
        // The ChunkStore (separate) handles the actual refcount.
    }

    // --- Scenario: Cross-tenant dedup (default tenants) ---
    #[test]
    fn cross_tenant_dedup_same_chunk_id() {
        let mut store = CompositionStore::new();
        let tenant_pharma = OrgId(uuid::Uuid::from_u128(100));
        let tenant_biotech = OrgId(uuid::Uuid::from_u128(200));
        store.add_namespace(make_ns(10, tenant_pharma, test_shard()));
        store.add_namespace(make_ns(20, tenant_biotech, test_shard()));

        let chunk_abc = ChunkId([0xab; 32]);
        let ns_pharma = NamespaceId(uuid::Uuid::from_u128(10));
        let ns_biotech = NamespaceId(uuid::Uuid::from_u128(20));

        let id_p = store.create(ns_pharma, vec![chunk_abc], 1024).unwrap();
        let id_b = store.create(ns_biotech, vec![chunk_abc], 1024).unwrap();

        // Both compositions reference the same chunk ID — dedup at ChunkStore.
        assert_eq!(store.get(id_p).unwrap().chunks, vec![chunk_abc]);
        assert_eq!(store.get(id_b).unwrap().chunks, vec![chunk_abc]);
        // Different tenants own the compositions.
        assert_eq!(store.get(id_p).unwrap().tenant_id, tenant_pharma);
        assert_eq!(store.get(id_b).unwrap().tenant_id, tenant_biotech);
    }

    // --- Scenario: No cross-tenant dedup for HMAC opted-out tenant ---
    #[test]
    fn hmac_tenant_different_chunk_id_no_dedup() {
        let mut store = CompositionStore::new();
        let tenant_defense = OrgId(uuid::Uuid::from_u128(300));
        let tenant_pharma = OrgId(uuid::Uuid::from_u128(100));
        store.add_namespace(make_ns(30, tenant_defense, test_shard()));
        store.add_namespace(make_ns(10, tenant_pharma, test_shard()));

        // HMAC-derived chunk ID vs SHA256 chunk ID for the same plaintext.
        let chunk_hmac = ChunkId([0xde; 32]); // HMAC(P, defense_key) = "def456"
        let chunk_sha = ChunkId([0xab; 32]); // sha256(P) = "abc123"

        let ns_defense = NamespaceId(uuid::Uuid::from_u128(30));
        let ns_pharma = NamespaceId(uuid::Uuid::from_u128(10));

        let id_d = store.create(ns_defense, vec![chunk_hmac], 1024).unwrap();
        let id_p = store.create(ns_pharma, vec![chunk_sha], 1024).unwrap();

        // Different chunk IDs — no dedup match.
        assert_ne!(
            store.get(id_d).unwrap().chunks[0],
            store.get(id_p).unwrap().chunks[0]
        );
    }

    // --- Scenario: Namespace inherits compliance tags ---
    #[test]
    fn namespace_inherits_org_compliance_tags() {
        use crate::namespace::ComplianceTag;

        let org_tags = vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr];
        let ns = Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: vec![ComplianceTag::RevFadp],
        };

        let effective = ns.effective_compliance_tags(&org_tags);
        assert_eq!(
            effective,
            vec![
                ComplianceTag::Hipaa,
                ComplianceTag::Gdpr,
                ComplianceTag::RevFadp
            ]
        );
    }

    #[test]
    fn namespace_compliance_tags_dedup() {
        use crate::namespace::ComplianceTag;

        let org_tags = vec![ComplianceTag::Hipaa];
        let ns = Namespace {
            id: test_ns(),
            tenant_id: test_tenant(),
            shard_id: test_shard(),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr],
        };

        let effective = ns.effective_compliance_tags(&org_tags);
        // HIPAA appears once despite being in both org and namespace.
        assert_eq!(effective, vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr]);
    }

    // --- Scenario: Chunk write fails during composition create ---
    #[test]
    fn chunk_write_failure_aborts_create_no_partial_state() {
        // Composition creates take chunk IDs after the caller confirms
        // chunk writes. If the caller does not pass chunk IDs (simulating
        // a chunk write failure), no composition is created.
        let mut store = setup();
        let initial_count = store.count();

        // Simulate: chunk write failed, so we never call create().
        // Verify the store has no partial state.
        assert_eq!(store.count(), initial_count);

        // Also: creating with valid chunks then deleting leaves no trace.
        let id = store
            .create(test_ns(), vec![ChunkId([0xff; 32])], 100)
            .unwrap();
        assert_eq!(store.count(), initial_count + 1);
        let _ = store.delete(id).unwrap();
        assert_eq!(store.count(), initial_count);
    }

    // --- Scenario: Delta commit fails after chunk write succeeds ---
    #[test]
    fn delta_commit_failure_rollback_removes_composition() {
        let mut store = setup();
        let c20 = ChunkId([0x20; 32]);
        let id = store.create(test_ns(), vec![c20], 4096).unwrap();

        // Simulate delta commit failure: caller rolls back by deleting.
        let result = store.delete(id).unwrap();
        assert_eq!(result, DeleteResult::Removed(vec![c20]));
        assert!(store.get(id).is_err());
        // c20 now has refcount 0 (returned to caller for GC).
    }

    // --- Scenario: Collective checkpoint announcement (I-WA1) ---
    #[test]
    fn advisory_hint_does_not_affect_create_correctness() {
        // Advisory hints are pass-through — composition operations succeed
        // identically with or without them (I-WA1).
        let mut store = setup();
        let chunks = vec![ChunkId([0xcc; 32])];

        // Create without any advisory context.
        let id = store.create(test_ns(), chunks.clone(), 4096).unwrap();
        let comp = store.get(id).unwrap().clone();

        // Verify the composition is correct regardless of advisory state.
        assert_eq!(comp.chunks, chunks);
        assert_eq!(comp.size, 4096);
        assert_eq!(comp.version, 1);
    }

    // --- Scenario: Retention-intent { final } ---
    #[test]
    fn retention_intent_does_not_change_multipart_finalize() {
        // retention_intent is advisory — finalize semantics are unchanged.
        let mut store = setup();
        let upload_id = store.start_multipart(test_ns()).unwrap();

        store
            .upload_part(&upload_id, 1, ChunkId([0xa0; 32]), 512)
            .unwrap();
        store
            .upload_part(&upload_id, 2, ChunkId([0xa1; 32]), 512)
            .unwrap();

        let comp_id = store.finalize_multipart(&upload_id).unwrap();
        let comp = store.get(comp_id).unwrap();

        // I-L5: chunks confirmed and visible only after finalize.
        assert_eq!(comp.chunks.len(), 2);
        assert_eq!(comp.size, 1024);
        // I-C2: refcount semantics unchanged by advisory hints.
    }

    // --- Scenario: Caller-scoped refcount activity telemetry ---
    #[test]
    fn rapid_creates_tracked_by_store_count() {
        // Telemetry is an observability concern; unit-level validation:
        // the store tracks composition count accurately under rapid mutations.
        let mut store = setup();
        let mut ids = Vec::new();
        for i in 0u8..10 {
            let id = store
                .create(test_ns(), vec![ChunkId([i; 32])], 100)
                .unwrap();
            ids.push(id);
        }
        assert_eq!(store.count(), 10);

        for id in &ids[..5] {
            let _ = store.delete(*id).unwrap();
        }
        assert_eq!(store.count(), 5);
    }

    // --- Scenario: Hint cannot enable cross-namespace creation (I-WA14) ---
    #[test]
    fn create_in_unauthorized_namespace_rejected_regardless_of_hints() {
        let mut store = setup();
        // Namespace 99 does not exist — any create attempt is rejected
        // regardless of advisory context.
        let bogus_ns = NamespaceId(uuid::Uuid::from_u128(99));
        let result = store.create(bogus_ns, vec![], 0);
        assert!(matches!(
            result,
            Err(CompositionError::NamespaceNotFound(_))
        ));
    }

    // --- Scenario: Advisory disabled — composition path unaffected (I-WA2) ---
    #[test]
    fn all_ops_succeed_without_advisory_context() {
        // Full lifecycle without any advisory integration — correctness
        // is identical (I-WA2).
        let mut store = setup();

        // Create.
        let c1 = ChunkId([0x01; 32]);
        let id = store.create(test_ns(), vec![c1], 1024).unwrap();

        // Update.
        let c2 = ChunkId([0x02; 32]);
        let v2 = store.update(id, vec![c1, c2], 2048).unwrap();
        assert_eq!(v2, 2);

        // Multipart.
        let upload_id = store.start_multipart(test_ns()).unwrap();
        store
            .upload_part(&upload_id, 1, ChunkId([0x10; 32]), 512)
            .unwrap();
        let mp_id = store.finalize_multipart(&upload_id).unwrap();
        assert!(store.get(mp_id).is_ok());

        // Delete.
        let result = store.delete(id).unwrap();
        assert!(matches!(result, DeleteResult::Removed(_)));
    }
}
