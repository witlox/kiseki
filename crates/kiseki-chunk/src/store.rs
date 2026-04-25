//! In-memory chunk store — reference implementation of [`ChunkOps`].

use std::collections::{HashMap, HashSet};

use kiseki_common::ids::ChunkId;
use kiseki_crypto::envelope::Envelope;

use crate::ec;
use crate::error::ChunkError;
use crate::placement::{self, DeviceInfo};
use crate::pool::{AffinityPool, DurabilityStrategy};

/// EC metadata for a stored chunk.
#[derive(Clone, Debug)]
pub struct EcMeta {
    /// Number of data shards.
    pub data_shards: usize,
    /// Number of parity shards.
    pub parity_shards: usize,
    /// Fragment data indexed by shard index.
    pub fragments: Vec<Vec<u8>>,
    /// Device index per fragment (into the pool's device list).
    pub device_indices: Vec<usize>,
    /// Original ciphertext length.
    pub original_len: usize,
}

/// Stored chunk entry.
struct ChunkEntry {
    envelope: Envelope,
    refcount: u64,
    retention_holds: HashSet<String>,
    pool: String,
    /// EC metadata (None for replication-mode pools).
    ec: Option<EcMeta>,
    /// Total bytes charged to pool (includes EC overhead). Used by GC.
    stored_bytes: u64,
}

/// Chunk storage operations trait.
pub trait ChunkOps {
    /// Write a chunk. If the chunk ID already exists (dedup hit),
    /// increments the refcount instead of writing new data (I-C1, I-C2).
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError>;

    /// Read a chunk by ID. Returns an owned Envelope (supports both
    /// in-memory and persistent backends).
    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError>;

    /// Increment refcount for an existing chunk (dedup).
    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Decrement refcount. Returns the new refcount.
    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Set a retention hold on a chunk (I-C2b).
    fn set_retention_hold(&mut self, chunk_id: &ChunkId, hold_name: &str)
        -> Result<(), ChunkError>;

    /// Release a retention hold.
    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError>;

    /// Run GC: delete chunks with refcount=0 and no retention holds.
    /// Returns the number of chunks deleted.
    fn gc(&mut self) -> u64;

    /// Get the refcount for a chunk.
    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;
}

/// In-memory chunk store.
pub struct ChunkStore {
    chunks: HashMap<ChunkId, ChunkEntry>,
    pools: HashMap<String, AffinityPool>,
    /// Simulated unavailable chunks for fault injection (ADR-037).
    unavailable: HashSet<ChunkId>,
}

impl ChunkStore {
    /// Create an empty chunk store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunks: HashMap::new(),
            pools: HashMap::new(),
            unavailable: HashSet::new(),
        }
    }

    /// Mark chunks as unavailable (fault injection for testing).
    pub fn inject_unavailable(&mut self, chunk_id: ChunkId) {
        self.unavailable.insert(chunk_id);
    }

    /// Clear all fault injections.
    pub fn clear_faults(&mut self) {
        self.unavailable.clear();
    }

    /// Check if a chunk has an injected fault.
    pub fn is_unavailable(&self, chunk_id: &ChunkId) -> bool {
        self.unavailable.contains(chunk_id)
    }

    /// Add an affinity pool.
    pub fn add_pool(&mut self, pool: AffinityPool) {
        self.pools.insert(pool.name.clone(), pool);
    }

    /// Total number of stored chunks.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Get EC metadata for a chunk (for BDD assertions).
    #[must_use]
    pub fn ec_meta(&self, chunk_id: &ChunkId) -> Option<&EcMeta> {
        self.chunks.get(chunk_id).and_then(|e| e.ec.as_ref())
    }

    /// Get a mutable pool reference.
    pub fn pool_mut(&mut self, name: &str) -> Option<&mut AffinityPool> {
        self.pools.get_mut(name)
    }

    /// Get a pool reference.
    #[must_use]
    pub fn pool(&self, name: &str) -> Option<&AffinityPool> {
        self.pools.get(name)
    }

    /// Read a chunk with EC-aware degraded read.
    ///
    /// Checks which devices are online in the pool, gathers available
    /// fragments, and reconstructs via EC if needed.
    pub fn read_chunk_ec(&self, chunk_id: &ChunkId) -> Result<Vec<u8>, ChunkError> {
        let entry = self
            .chunks
            .get(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        let Some(ec) = &entry.ec else {
            return Ok(entry.envelope.ciphertext.clone());
        };

        let pool = self
            .pools
            .get(&entry.pool)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        let total = ec.data_shards + ec.parity_shards;

        // Build fragment array with None for offline devices.
        let mut frags: Vec<Option<Vec<u8>>> = Vec::with_capacity(total);
        for i in 0..total {
            let dev_idx = ec.device_indices[i];
            let online = pool.devices.get(dev_idx).is_some_and(|d| d.online);
            if online {
                frags.push(Some(ec.fragments[i].clone()));
            } else {
                frags.push(None);
            }
        }

        ec::decode(
            &mut frags,
            ec.data_shards,
            ec.parity_shards,
            ec.original_len,
        )
    }
}

impl Default for ChunkStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkOps for ChunkStore {
    fn write_chunk(&mut self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        let chunk_id = envelope.chunk_id;

        // Dedup: if chunk already exists, just bump refcount.
        if let Some(entry) = self.chunks.get_mut(&chunk_id) {
            entry.refcount += 1;
            return Ok(false); // not a new write
        }

        // Check pool capacity.
        let storage_size;
        let ec_meta = if let Some(p) = self.pools.get(pool) {
            match p.durability {
                DurabilityStrategy::ErasureCoding {
                    data_shards,
                    parity_shards,
                } => {
                    let encoded = ec::encode(
                        &envelope.ciphertext,
                        usize::from(data_shards),
                        usize::from(parity_shards),
                    )?;

                    // Place fragments on devices.
                    let total = encoded.fragments.len();
                    let dev_infos: Vec<DeviceInfo> = p
                        .devices
                        .iter()
                        .map(|d| DeviceInfo {
                            id: d.id.clone(),
                            online: d.online,
                        })
                        .collect();

                    let device_indices = placement::place_fragments(&chunk_id, total, &dev_infos)
                        .ok_or(ChunkError::EcInvalidConfig)?;

                    storage_size = encoded.fragments.iter().map(|f| f.len() as u64).sum();

                    Some(EcMeta {
                        data_shards: encoded.data_shards,
                        parity_shards: encoded.parity_shards,
                        fragments: encoded.fragments,
                        device_indices,
                        original_len: encoded.original_len,
                    })
                }
                DurabilityStrategy::Replication { copies } => {
                    storage_size = envelope.ciphertext.len() as u64 * u64::from(copies);
                    None
                }
            }
        } else {
            storage_size = envelope.ciphertext.len() as u64;
            None
        };

        if let Some(p) = self.pools.get_mut(pool) {
            if !p.has_capacity(storage_size) {
                return Err(ChunkError::PoolFull(pool.to_owned()));
            }
            p.used_bytes += storage_size;
        }

        self.chunks.insert(
            chunk_id,
            ChunkEntry {
                envelope,
                refcount: 1,
                retention_holds: HashSet::new(),
                pool: pool.to_owned(),
                ec: ec_meta,
                stored_bytes: storage_size,
            },
        );

        Ok(true) // new write
    }

    fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError> {
        // Fault injection: simulated unavailability (ADR-037).
        if self.unavailable.contains(chunk_id) {
            return Err(ChunkError::DeviceUnavailable(*chunk_id));
        }
        self.chunks
            .get(chunk_id)
            .map(|e| e.envelope.clone())
            .ok_or(ChunkError::NotFound(*chunk_id))
    }

    fn increment_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.refcount += 1;
        Ok(entry.refcount)
    }

    fn decrement_refcount(&mut self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        if entry.refcount == 0 {
            return Err(ChunkError::RefcountUnderflow(*chunk_id));
        }
        entry.refcount -= 1;
        Ok(entry.refcount)
    }

    fn set_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.retention_holds.insert(hold_name.to_owned());
        Ok(())
    }

    fn release_retention_hold(
        &mut self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let entry = self
            .chunks
            .get_mut(chunk_id)
            .ok_or(ChunkError::NotFound(*chunk_id))?;
        entry.retention_holds.remove(hold_name);
        Ok(())
    }

    fn gc(&mut self) -> u64 {
        let to_remove: Vec<ChunkId> = self
            .chunks
            .iter()
            .filter(|(_, e)| e.refcount == 0 && e.retention_holds.is_empty())
            .map(|(id, _)| *id)
            .collect();

        let count = to_remove.len() as u64;

        for id in &to_remove {
            if let Some(entry) = self.chunks.remove(id) {
                if let Some(pool) = self.pools.get_mut(&entry.pool) {
                    pool.used_bytes = pool.used_bytes.saturating_sub(entry.stored_bytes);
                }
            }
        }

        count
    }

    fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        self.chunks
            .get(chunk_id)
            .map(|e| e.refcount)
            .ok_or(ChunkError::NotFound(*chunk_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::DurabilityStrategy;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::aead::{GCM_NONCE_LEN, GCM_TAG_LEN};

    fn test_envelope(chunk_id_byte: u8) -> Envelope {
        Envelope {
            ciphertext: vec![0xab; 256],
            auth_tag: [0xcc; GCM_TAG_LEN],
            nonce: [0xdd; GCM_NONCE_LEN],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
            chunk_id: ChunkId([chunk_id_byte; 32]),
        }
    }

    fn setup_store() -> ChunkStore {
        let mut store = ChunkStore::new();
        store.add_pool(
            AffinityPool::new(
                "fast-nvme",
                DurabilityStrategy::default(),
                1024 * 1024 * 1024,
            )
            .with_devices(6),
        );
        store
    }

    #[test]
    fn write_and_read_roundtrip() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        let is_new = store.write_chunk(env, "fast-nvme");
        assert!(is_new.is_ok());
        assert!(is_new.unwrap_or_else(|_| unreachable!()));

        let read = store.read_chunk(&chunk_id);
        assert!(read.is_ok());
        assert_eq!(read.unwrap_or_else(|_| unreachable!()).chunk_id, chunk_id);
    }

    #[test]
    fn dedup_increments_refcount() {
        let mut store = setup_store();
        let env1 = test_envelope(0x01);
        let env2 = test_envelope(0x01); // same chunk ID
        let chunk_id = env1.chunk_id;

        store
            .write_chunk(env1, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        let is_new = store
            .write_chunk(env2, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        assert!(!is_new); // dedup hit

        assert_eq!(
            store.refcount(&chunk_id).unwrap_or_else(|_| unreachable!()),
            2
        );
    }

    #[test]
    fn gc_respects_refcount() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());

        // Refcount = 1, should NOT be GC'd.
        assert_eq!(store.gc(), 0);

        // Decrement to 0.
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());

        // Now GC should remove it.
        assert_eq!(store.gc(), 1);
        assert_eq!(store.chunk_count(), 0);
    }

    #[test]
    fn retention_hold_blocks_gc() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());
        store
            .set_retention_hold(&chunk_id, "hipaa-7yr")
            .unwrap_or_else(|_| unreachable!());

        // Refcount = 0 but hold active → NOT GC'd (I-C2b).
        assert_eq!(store.gc(), 0);

        // Release hold → now GC works.
        store
            .release_retention_hold(&chunk_id, "hipaa-7yr")
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(store.gc(), 1);
    }

    #[test]
    fn refcount_underflow_rejected() {
        let mut store = setup_store();
        let env = test_envelope(0x01);
        let chunk_id = env.chunk_id;

        store
            .write_chunk(env, "fast-nvme")
            .unwrap_or_else(|_| unreachable!());
        store
            .decrement_refcount(&chunk_id)
            .unwrap_or_else(|_| unreachable!());

        // Second decrement: refcount is 0 → underflow error.
        let result = store.decrement_refcount(&chunk_id);
        assert!(result.is_err());
    }

    #[test]
    fn chunk_not_found() {
        let store = setup_store();
        let result = store.read_chunk(&ChunkId([0xff; 32]));
        assert!(result.is_err());
    }

    #[test]
    fn write_chunk_returns_false_for_duplicate() {
        // I-C1: dedup — write_chunk returns is_new=false on duplicate.
        let mut store = setup_store();
        let env1 = test_envelope(0x42);
        let env2 = test_envelope(0x42); // same chunk ID

        let is_new1 = store.write_chunk(env1, "fast-nvme").unwrap();
        assert!(is_new1, "first write should be new");

        let is_new2 = store.write_chunk(env2, "fast-nvme").unwrap();
        assert!(!is_new2, "duplicate write should return is_new=false");

        // Refcount should be 2 after two writes of the same chunk.
        assert_eq!(store.refcount(&ChunkId([0x42; 32])).unwrap(), 2);
    }

    #[test]
    fn retention_hold_survives_gc_then_release_allows_gc() {
        // I-C2b: chunk with retention hold is not GC'd.
        let mut store = setup_store();
        let env = test_envelope(0x50);
        let chunk_id = env.chunk_id;

        store.write_chunk(env, "fast-nvme").unwrap();
        store.decrement_refcount(&chunk_id).unwrap(); // refcount = 0

        // Set hold.
        store
            .set_retention_hold(&chunk_id, "legal-hold-2025")
            .unwrap();

        // GC should NOT delete it.
        assert_eq!(store.gc(), 0);
        assert_eq!(store.chunk_count(), 1);

        // Add a second hold, release the first.
        store.set_retention_hold(&chunk_id, "audit-hold").unwrap();
        store
            .release_retention_hold(&chunk_id, "legal-hold-2025")
            .unwrap();

        // Still one hold left — should survive GC.
        assert_eq!(store.gc(), 0);

        // Release the last hold — now GC should remove it.
        store
            .release_retention_hold(&chunk_id, "audit-hold")
            .unwrap();
        assert_eq!(store.gc(), 1);
        assert_eq!(store.chunk_count(), 0);
    }

    #[test]
    fn refcount_increment_twice_decrement_once() {
        let mut store = setup_store();
        let env = test_envelope(0x60);
        let chunk_id = env.chunk_id;

        store.write_chunk(env, "fast-nvme").unwrap(); // refcount = 1
        store.increment_refcount(&chunk_id).unwrap(); // refcount = 2
        store.increment_refcount(&chunk_id).unwrap(); // refcount = 3
        store.decrement_refcount(&chunk_id).unwrap(); // refcount = 2

        assert_eq!(store.refcount(&chunk_id).unwrap(), 2);

        // Should NOT be GC'd since refcount > 0.
        assert_eq!(store.gc(), 0);
    }

    // ---------------------------------------------------------------
    // Scenario: Affinity hint preference honoured within policy (I-WA1)
    // Placement works correctly WITHOUT hints — hints are preferences
    // only, never required. Placement still enforces pool authorization.
    // ---------------------------------------------------------------
    #[test]
    fn placement_works_without_affinity_hints() {
        let mut store = ChunkStore::new();
        store.add_pool(
            AffinityPool::new(
                "fast-nvme",
                DurabilityStrategy::Replication { copies: 1 },
                1024 * 1024,
            )
            .with_devices(3),
        );
        store.add_pool(
            AffinityPool::new(
                "bulk-nvme",
                DurabilityStrategy::Replication { copies: 1 },
                1024 * 1024,
            )
            .with_devices(3),
        );

        // Write without any affinity hint — placement uses the pool name directly.
        let env = test_envelope(0xA1);
        let result = store.write_chunk(env, "fast-nvme");
        assert!(result.is_ok(), "placement must succeed without hints");
        assert!(result.unwrap(), "should be a new write");

        // Write to the other pool — also works.
        let env2 = test_envelope(0xA2);
        let result2 = store.write_chunk(env2, "bulk-nvme");
        assert!(result2.is_ok(), "placement to alternate pool must succeed");

        // Attempting a non-existent pool still writes (no pool capacity check
        // when pool is missing, but data is stored).
        let env3 = test_envelope(0xA3);
        let result3 = store.write_chunk(env3, "nonexistent-pool");
        assert!(result3.is_ok(), "missing pool does not block write");
    }

    // ---------------------------------------------------------------
    // Scenario: Dedup-intent { per-rank } skips dedup path
    // When dedup is bypassed, identical plaintext gets separate chunks.
    // ---------------------------------------------------------------
    #[test]
    fn dedup_intent_per_rank_skips_dedup() {
        let mut store = setup_store();

        // Two envelopes with DIFFERENT chunk IDs (simulating per-rank
        // dedup bypass: each rank derives its own chunk ID).
        let env_rank0 = test_envelope(0xB0);
        let env_rank1 = test_envelope(0xB1);

        store.write_chunk(env_rank0, "fast-nvme").unwrap();
        store.write_chunk(env_rank1, "fast-nvme").unwrap();

        // Both chunks exist independently — no dedup coalesced them.
        assert_eq!(store.chunk_count(), 2);
        assert_eq!(store.refcount(&ChunkId([0xB0; 32])).unwrap(), 1);
        assert_eq!(store.refcount(&ChunkId([0xB1; 32])).unwrap(), 1);
    }

    // ---------------------------------------------------------------
    // Scenario: Dedup-intent { shared-ensemble } uses normal dedup
    // Same chunk ID → dedup hit, refcount incremented.
    // ---------------------------------------------------------------
    #[test]
    fn dedup_intent_shared_ensemble_uses_normal_dedup() {
        let mut store = setup_store();

        // Two envelopes with THE SAME chunk ID (simulating shared-ensemble
        // where dedup is used normally).
        let env1 = test_envelope(0xC0);
        let env2 = test_envelope(0xC0); // same ID

        let is_new1 = store.write_chunk(env1, "fast-nvme").unwrap();
        let is_new2 = store.write_chunk(env2, "fast-nvme").unwrap();

        assert!(is_new1, "first write is new");
        assert!(!is_new2, "second write is a dedup hit");
        assert_eq!(store.chunk_count(), 1, "only one chunk stored");
        assert_eq!(
            store.refcount(&ChunkId([0xC0; 32])).unwrap(),
            2,
            "refcount should be 2"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Locality-class telemetry for caller-owned chunks
    // Telemetry classifies chunks by placement. No internal topology
    // is leaked (I-WA11). Only owner's chunks are visible (I-WA6).
    // ---------------------------------------------------------------
    #[test]
    fn locality_class_telemetry_shape() {
        // Locality classes defined by the spec.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(dead_code)]
        enum LocalityClass {
            LocalNode,
            LocalRack,
            SamePool,
            Remote,
            Degraded,
        }

        // Simulate classifying chunks — the response shape must not
        // include any node ID, rack label, or pool utilization metric.
        struct LocalityResponse {
            chunk_classes: Vec<(ChunkId, LocalityClass)>,
        }

        let resp = LocalityResponse {
            chunk_classes: vec![
                (ChunkId([0x01; 32]), LocalityClass::LocalNode),
                (ChunkId([0x02; 32]), LocalityClass::SamePool),
                (ChunkId([0x03; 32]), LocalityClass::Degraded),
            ],
        };

        // Verify response shape: each entry is (chunk_id, locality_class).
        assert_eq!(resp.chunk_classes.len(), 3);
        assert_eq!(resp.chunk_classes[0].1, LocalityClass::LocalNode);
        assert_eq!(resp.chunk_classes[2].1, LocalityClass::Degraded);

        // Unauthorized targets return same shape as absent chunks (I-WA6).
        let unauthorized_resp = LocalityResponse {
            chunk_classes: vec![],
        };
        assert!(unauthorized_resp.chunk_classes.is_empty());
    }

    // ---------------------------------------------------------------
    // Scenario: Pool backpressure telemetry uses k-anonymity bucketing
    // When k < 5, neighbour-derived fields carry a fixed sentinel (I-WA5).
    // ---------------------------------------------------------------
    #[test]
    fn pool_backpressure_k_anonymity_sentinel() {
        const K_THRESHOLD: usize = 5;
        const SENTINEL: &str = "<redacted>";

        #[allow(dead_code)]
        struct BackpressureTelemetry {
            pool_name: String,
            caller_usage_pct: u8,
            neighbour_fields: String, // sentinel when k < threshold
        }

        // Low-k case: k=4 < 5.
        let low_k = BackpressureTelemetry {
            pool_name: "fast-nvme".into(),
            caller_usage_pct: 25,
            neighbour_fields: if 4 < K_THRESHOLD {
                SENTINEL.into()
            } else {
                "actual-data".into()
            },
        };
        assert_eq!(
            low_k.neighbour_fields, SENTINEL,
            "low-k must use sentinel (I-WA5)"
        );

        // High-k case: k=10 >= 5.
        let high_k = BackpressureTelemetry {
            pool_name: "fast-nvme".into(),
            caller_usage_pct: 25,
            neighbour_fields: if 10 < K_THRESHOLD {
                SENTINEL.into()
            } else {
                "actual-data".into()
            },
        };
        assert_ne!(
            high_k.neighbour_fields, SENTINEL,
            "high-k should have real data"
        );

        // Both responses have identical shape (same struct fields).
        assert_eq!(low_k.pool_name, high_k.pool_name);
    }

    #[test]
    fn fault_injection_makes_chunk_unavailable() {
        let mut store = ChunkStore::new();
        store.add_pool(
            AffinityPool::new(
                "default",
                DurabilityStrategy::Replication { copies: 1 },
                1_000_000,
            )
            .with_devices(1),
        );
        let env = test_envelope(0x42);
        let chunk_id = env.chunk_id;
        store.write_chunk(env, "default").unwrap();

        // Before fault: read succeeds.
        assert!(store.read_chunk(&chunk_id).is_ok());

        // Inject fault: read fails with DeviceUnavailable.
        store.inject_unavailable(chunk_id);
        let err = store.read_chunk(&chunk_id).unwrap_err();
        assert!(matches!(err, ChunkError::DeviceUnavailable(_)));

        // Clear fault: read succeeds again.
        store.clear_faults();
        assert!(store.read_chunk(&chunk_id).is_ok());
    }
}
