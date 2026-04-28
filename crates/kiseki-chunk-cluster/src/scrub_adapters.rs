//! Production adapters for the [`crate::scrub_scheduler::ScrubScheduler`]
//! trait dependencies (Phase 16d step 4).
//!
//! - [`LocalChunkDeleter`] wraps an `Arc<dyn AsyncChunkOps>` and
//!   implements [`crate::OrphanDeleter`] by deleting every locally-
//!   held fragment of a confirmed-orphan chunk.
//! - [`FabricAvailabilityOracle`] wraps a peer list +
//!   `Vec<Arc<dyn FabricPeer>>` and implements
//!   [`crate::FragmentAvailabilityOracle`] via per-peer
//!   `HasFragment` probes.
//! - [`FabricRepairer`] wraps the same peer list and implements
//!   [`crate::Repairer`] via `GetFragment` from the healthy source
//!   + `PutFragment` to the missing destination.
//!
//! All three are thin (~30 LOC each); the heavy lifting lives in
//! the orchestrators and the scrub primitives shipped in 16b/16c.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kiseki_chunk::AsyncChunkOps;
use kiseki_common::ids::{ChunkId, OrgId};

use crate::peer::FabricPeer;
use crate::scrub::{FragmentAvailabilityOracle, OrphanDeleter, Repairer};

/// Production [`OrphanDeleter`] backed by an `AsyncChunkOps`. On
/// reclaim it lists every `fragment_index` held locally for the
/// chunk and deletes each one. Whole-envelope chunks aren't
/// reclaimed here (the orphan-fragment scrub specifically targets
/// fragments — whole envelopes always have a `cluster_chunk_state`
/// pairing because the gateway emits `ChunkAndDelta` on every write).
pub struct LocalChunkDeleter {
    local: Arc<dyn AsyncChunkOps>,
}

impl LocalChunkDeleter {
    /// Wrap a local store as a deleter.
    #[must_use]
    pub fn new(local: Arc<dyn AsyncChunkOps>) -> Self {
        Self { local }
    }
}

#[async_trait]
impl OrphanDeleter for LocalChunkDeleter {
    async fn delete(&self, chunk_id: ChunkId) -> Result<bool, String> {
        let fragments = self.local.list_fragments(&chunk_id).await;
        let mut deleted_any = false;
        for index in fragments {
            match self.local.delete_fragment(&chunk_id, index).await {
                Ok(true) => deleted_any = true,
                Ok(false) => {} // already absent (race with another scrub pass)
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(deleted_any)
    }
}

/// Production [`FragmentAvailabilityOracle`] that runs
/// `HasFragment` against every peer in the placement list. Peers
/// the oracle doesn't know about (placement entry not present in
/// the registered peer set) report `false`.
pub struct FabricAvailabilityOracle {
    /// Map of `node_id` → peer handle. Built once at construction
    /// time so per-call lookup is O(1).
    by_id: HashMap<u64, Arc<dyn FabricPeer>>,
}

impl FabricAvailabilityOracle {
    /// Build the oracle from the cluster's peer list. Each peer's
    /// name is parsed as `node-{id}` first, then `p{id}` (the
    /// `MockPeer` convention used in tests). Unparseable names are
    /// skipped.
    #[must_use]
    pub fn new(peers: &[Arc<dyn FabricPeer>]) -> Self {
        let mut by_id = HashMap::with_capacity(peers.len());
        for peer in peers {
            let name = peer.name();
            let id = name
                .strip_prefix("node-")
                .or_else(|| name.strip_prefix('p'))
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(id) = id {
                by_id.insert(id, Arc::clone(peer));
            }
        }
        Self { by_id }
    }
}

#[async_trait]
impl FragmentAvailabilityOracle for FabricAvailabilityOracle {
    async fn check(&self, chunk_id: ChunkId, peer_ids: &[u64]) -> Vec<bool> {
        let mut out = Vec::with_capacity(peer_ids.len());
        for (i, peer_id) in peer_ids.iter().enumerate() {
            let fragment_index = u32::try_from(i).unwrap_or(u32::MAX);
            let present = if let Some(peer) = self.by_id.get(peer_id) {
                peer.has_fragment(chunk_id, fragment_index).await.unwrap_or(false)
            } else {
                // Unknown peer id — treat as missing so the
                // under-replication scrub can repair it.
                false
            };
            out.push(present);
        }
        out
    }
}

/// Production [`Repairer`] that re-replicates a fragment by
/// pulling from `from_peer` via `GetFragment` and pushing to
/// `to_peer` via `PutFragment`. Tenant + pool come from
/// construction-time defaults.
pub struct FabricRepairer {
    by_id: HashMap<u64, Arc<dyn FabricPeer>>,
    tenant_id: OrgId,
    pool: String,
}

impl FabricRepairer {
    /// Build a repairer from the cluster's peer list. Same name
    /// parsing as [`FabricAvailabilityOracle`].
    #[must_use]
    pub fn new(peers: &[Arc<dyn FabricPeer>], tenant_id: OrgId, pool: String) -> Self {
        let mut by_id = HashMap::with_capacity(peers.len());
        for peer in peers {
            let name = peer.name();
            let id = name
                .strip_prefix("node-")
                .or_else(|| name.strip_prefix('p'))
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(id) = id {
                by_id.insert(id, Arc::clone(peer));
            }
        }
        Self {
            by_id,
            tenant_id,
            pool,
        }
    }
}

#[async_trait]
impl Repairer for FabricRepairer {
    async fn repair(
        &self,
        chunk_id: ChunkId,
        from_peer: u64,
        to_peer: u64,
    ) -> Result<(), String> {
        let src = self
            .by_id
            .get(&from_peer)
            .ok_or_else(|| format!("repair source peer {from_peer} unknown"))?;
        let dst = self
            .by_id
            .get(&to_peer)
            .ok_or_else(|| format!("repair destination peer {to_peer} unknown"))?;

        // Today's repair targets fragment_index 0 (Replication-N).
        // EC mode + multi-index repair lands when we wire the
        // under-replication scrub against EC fragments — which
        // requires storing fragment_index per placement entry, not
        // just node_id. Tracked as a 16e concern; for 16d the
        // Replication-N path is sufficient to verify the seam.
        let envelope = src
            .get_fragment(chunk_id, 0)
            .await
            .map_err(|e| format!("get_fragment: {e}"))?;
        dst.put_fragment(chunk_id, 0, self.tenant_id, self.pool.clone(), envelope)
            .await
            .map_err(|e| format!("put_fragment: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
    use kiseki_chunk::store::ChunkStore;
    use kiseki_chunk::SyncBridge;
    use kiseki_crypto::envelope::Envelope;

    use super::*;
    use crate::peer::FabricPeerError;

    fn cid(b: u8) -> ChunkId {
        ChunkId([b; 32])
    }

    fn local() -> Arc<dyn AsyncChunkOps> {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: "p".into(),
            device_class: DeviceClass::NvmeSsd,
            durability: DurabilityStrategy::Replication { copies: 1 },
            devices: vec![],
            capacity_bytes: 1 << 30,
            used_bytes: 0,
        });
        Arc::new(SyncBridge::new(store))
    }

    /// Phase 16d step 4: deleter walks `list_fragments` + drops each
    /// index. After the call `list_fragments` returns empty for the
    /// chunk.
    #[tokio::test]
    async fn local_chunk_deleter_drops_every_fragment_index() {
        let store = local();
        store.write_fragment(&cid(0xA1), 0, b"f0".to_vec()).await.unwrap();
        store.write_fragment(&cid(0xA1), 2, b"f2".to_vec()).await.unwrap();
        store.write_fragment(&cid(0xA1), 5, b"f5".to_vec()).await.unwrap();

        let deleter = LocalChunkDeleter::new(Arc::clone(&store));
        let removed = deleter.delete(cid(0xA1)).await.expect("delete");
        assert!(removed, "should report at least one fragment deleted");

        assert!(
            store.list_fragments(&cid(0xA1)).await.is_empty(),
            "all fragments must be gone after deleter sweep",
        );
    }

    /// Idempotent delete — no fragments, no-op, returns false.
    #[tokio::test]
    async fn local_chunk_deleter_is_idempotent_on_absent() {
        let store = local();
        let deleter = LocalChunkDeleter::new(store);
        let removed = deleter.delete(cid(0xBA)).await.expect("absent");
        assert!(!removed, "no fragments → reports not-removed");
    }

    /// Test peer that only answers `has_fragment` + `delete`/`put` for
    /// the `FabricRepairer` integration test.
    struct TestPeer {
        name: &'static str,
        present: Mutex<bool>,
        has_calls: AtomicU64,
        last_put_chunk: Mutex<Option<ChunkId>>,
    }
    impl TestPeer {
        fn new(name: &'static str, present: bool) -> Arc<Self> {
            Arc::new(Self {
                name,
                present: Mutex::new(present),
                has_calls: AtomicU64::new(0),
                last_put_chunk: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl FabricPeer for TestPeer {
        fn name(&self) -> &str {
            self.name
        }
        async fn put_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
            _tenant_id: OrgId,
            _pool_id: String,
            _envelope: Envelope,
        ) -> Result<bool, FabricPeerError> {
            *self.last_put_chunk.lock().unwrap() = Some(chunk_id);
            *self.present.lock().unwrap() = true;
            Ok(true)
        }
        async fn get_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
        ) -> Result<Envelope, FabricPeerError> {
            if !*self.present.lock().unwrap() {
                return Err(FabricPeerError::NotFound);
            }
            Ok(Envelope {
                chunk_id,
                ciphertext: b"repaired".to_vec(),
                auth_tag: [0u8; 16],
                nonce: [0u8; 12],
                system_epoch: kiseki_common::tenancy::KeyEpoch(1),
                tenant_epoch: None,
                tenant_wrapped_material: None,
            })
        }
        async fn delete_fragment(
            &self,
            _chunk_id: ChunkId,
            _fragment_index: u32,
            _tenant_id: OrgId,
        ) -> Result<bool, FabricPeerError> {
            Ok(true)
        }
        async fn has_fragment(
            &self,
            _chunk_id: ChunkId,
            _fragment_index: u32,
        ) -> Result<bool, FabricPeerError> {
            self.has_calls.fetch_add(1, Ordering::SeqCst);
            Ok(*self.present.lock().unwrap())
        }
    }

    /// Phase 16d step 4: oracle reports `[true, false, true]` when
    /// the corresponding peers report present / absent / present.
    /// Uses `MockPeer`'s "p{N}" naming so the `by_id` parser hits.
    #[tokio::test]
    async fn fabric_availability_oracle_aggregates_per_peer_presence() {
        let p1 = TestPeer::new("p1", true);
        let p2 = TestPeer::new("p2", false);
        let p3 = TestPeer::new("p3", true);
        let peers: Vec<Arc<dyn FabricPeer>> = vec![
            Arc::clone(&p1) as _,
            Arc::clone(&p2) as _,
            Arc::clone(&p3) as _,
        ];
        let oracle = FabricAvailabilityOracle::new(&peers);
        let presence = oracle.check(cid(0xC1), &[1, 2, 3]).await;
        assert_eq!(presence, vec![true, false, true]);
    }

    /// Phase 16d step 4: a placement entry naming a peer the
    /// oracle wasn't told about reports `false` (the under-
    /// replication scrub then plans to repair it onto a healthy
    /// peer).
    #[tokio::test]
    async fn fabric_availability_oracle_returns_false_for_unknown_peers() {
        let p1 = TestPeer::new("p1", true);
        let peers: Vec<Arc<dyn FabricPeer>> = vec![Arc::clone(&p1) as _];
        let oracle = FabricAvailabilityOracle::new(&peers);
        let presence = oracle.check(cid(0xD1), &[1, 99]).await;
        assert_eq!(presence, vec![true, false]);
    }

    /// Phase 16d step 4: repairer pulls from `from_peer` via
    /// `GetFragment` and pushes to `to_peer` via `PutFragment`. Both
    /// sides see the right calls.
    #[tokio::test]
    async fn fabric_repairer_round_trips_get_then_put() {
        let p_src = TestPeer::new("p1", true); // has the fragment
        let p_dst = TestPeer::new("p2", false); // missing
        let peers: Vec<Arc<dyn FabricPeer>> = vec![
            Arc::clone(&p_src) as _,
            Arc::clone(&p_dst) as _,
        ];
        let repairer = FabricRepairer::new(&peers, OrgId(uuid::Uuid::nil()), "p".into());
        repairer
            .repair(cid(0xE1), 1, 2)
            .await
            .expect("repair ok");
        assert_eq!(
            *p_dst.last_put_chunk.lock().unwrap(),
            Some(cid(0xE1)),
            "destination saw a put for the right chunk_id"
        );
        assert!(
            *p_dst.present.lock().unwrap(),
            "destination now reports present after the repair",
        );
    }
}
