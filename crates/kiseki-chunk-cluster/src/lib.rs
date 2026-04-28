//! Cross-node chunk replication for Kiseki.
//!
//! Phase 16a — D-1, D-5, D-6, D-7, D-10. The [`ClusteredChunkStore`]
//! wraps a local [`AsyncChunkOps`] (typically a `SyncBridge<ChunkStore>`)
//! and fans fragments out to peer nodes via the [`FabricPeer`] trait
//! (the gRPC `ClusterChunkService` client lives behind this trait so
//! the store stays unit-testable with mock peers).
//!
//! ## Replication model (16a)
//!
//! Only **Replication-N** (N=3 default) is shipped in 16a. Each peer
//! holds the **whole envelope** at `fragment_index = 0`. EC fragment
//! distribution lands in 16b.
//!
//! ## Write semantics — D-5 quorum
//!
//! ```text
//!   write_chunk(envelope, pool):
//!     1. local AsyncChunkOps.write_chunk             ← 1 of N acks
//!     2. fan out PutFragment to all peers in parallel (5s/peer)
//!     3. wait until total acks ≥ min_acks            ← typically 2-of-3
//!     4. return Ok                                    ← then caller
//!                                                       proposes the
//!                                                       CombinedProposal
//!                                                       to Raft (D-4).
//!     5. on quorum failure → Err(ChunkError::QuorumLost)
//! ```
//!
//! The ack-after-Raft-commit invariant (I-L2) is NOT enforced inside
//! this crate; the caller (the gateway / control plane wiring done in
//! step 7) submits the [`CombinedProposal`][cp] *after* `write_chunk`
//! returns and only acks the client after Raft commit.
//!
//! [cp]: kiseki_log::raft_store::LogCommand::ChunkAndDelta
//!
//! ## Read semantics — D-10 cross-stream ordering
//!
//! ```text
//!   read_chunk(chunk_id):
//!     1. try local AsyncChunkOps.read_chunk
//!     2. on NotFound: walk peer list, GetFragment (3s/peer)
//!     3. first peer to return Ok wins
//!     4. on all-fail: NotFound (caller maps to NFS4ERR_DELAY)
//! ```
//!
//! Spec: `specs/implementation/phase-16-cross-node-chunks.md` (rev 4)
//! ADR-005 ec-and-chunk-durability, ADR-026 raft-topology
//! Invariants: I-C2, I-C4, I-D1, I-T1, I-L2, I-L5

#![deny(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kiseki_chunk::{AsyncChunkOps, ChunkError};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_crypto::envelope::Envelope;

pub mod auth;
pub mod defaults;
pub mod metrics;
pub mod peer;
pub mod server;

pub use auth::{verify_fabric_san, FabricAuthError};
pub use defaults::{defaults_for, ClusterDurabilityDefaults};
pub use metrics::FabricMetrics;
pub use peer::{FabricPeer, FabricPeerError, GrpcFabricPeer};
pub use server::{fabric_san_interceptor, ClusterChunkServer};

/// Default per-peer timeout for `PutFragment` (write-side fan-out).
pub const DEFAULT_PUT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default per-peer timeout for `GetFragment` (read-side fallback).
pub const DEFAULT_GET_TIMEOUT: Duration = Duration::from_secs(3);

/// Configuration for a [`ClusteredChunkStore`].
#[derive(Clone)]
pub struct ClusterCfg {
    /// Tenant ID propagated on `PutFragment` so the receiving peer
    /// can route to the correct affinity pool even under cross-stream
    /// reordering (D-10).
    pub tenant_id: OrgId,
    /// Pool name string passed through to the local store. The proto
    /// `AffinityPoolId` mapping happens at the gRPC client wrapper
    /// (the `peer::grpc` impl in step 5).
    pub pool: String,
    /// Minimum total acks required to consider a write durable.
    /// `local + remote_acks ≥ min_acks` ⇒ success. Default: 2 (for
    /// the 3-node Replication-3 baseline; matches I-L2 majority).
    pub min_acks: usize,
    /// Per-peer timeout on `PutFragment`.
    pub put_timeout: Duration,
    /// Per-peer timeout on `GetFragment`.
    pub get_timeout: Duration,
}

impl ClusterCfg {
    /// Build a default cfg for a tenant + pool.
    #[must_use]
    pub fn new(tenant_id: OrgId, pool: impl Into<String>) -> Self {
        Self {
            tenant_id,
            pool: pool.into(),
            min_acks: 2,
            put_timeout: DEFAULT_PUT_TIMEOUT,
            get_timeout: DEFAULT_GET_TIMEOUT,
        }
    }

    /// Override `min_acks` (Phase 16b step 3 — runtime sets this from
    /// the per-cluster-size defaults table).
    #[must_use]
    pub fn with_min_acks(mut self, min_acks: usize) -> Self {
        self.min_acks = min_acks;
        self
    }
}

/// `AsyncChunkOps` implementation that fans writes out to peer nodes
/// via [`FabricPeer`] and falls back to peer reads on local miss.
///
/// `peers` is the list of *remote* peers — the local node is not in
/// this list. When `peers.is_empty()` the store degenerates to
/// local-only (D-6 single-node compatibility).
pub struct ClusteredChunkStore {
    local: Arc<dyn AsyncChunkOps>,
    peers: Vec<Arc<dyn FabricPeer>>,
    cfg: ClusterCfg,
    metrics: Option<Arc<FabricMetrics>>,
}

impl ClusteredChunkStore {
    /// Wire a local async store + a list of peers.
    #[must_use]
    pub fn new(
        local: Arc<dyn AsyncChunkOps>,
        peers: Vec<Arc<dyn FabricPeer>>,
        cfg: ClusterCfg,
    ) -> Self {
        Self {
            local,
            peers,
            cfg,
            metrics: None,
        }
    }

    /// Attach a [`FabricMetrics`] — fabric ops will be recorded.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<FabricMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Total replication factor (1 local + N peers).
    #[must_use]
    pub fn replication_factor(&self) -> usize {
        1 + self.peers.len()
    }

    fn quorum_required(&self) -> usize {
        // Cap min_acks at the actual replication factor — a 1-node
        // cluster with min_acks=2 should not deadlock; degenerate to
        // local-only success per D-6.
        self.cfg.min_acks.min(self.replication_factor())
    }
}

#[async_trait]
impl AsyncChunkOps for ClusteredChunkStore {
    async fn write_chunk(&self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        // 1. Local write — counts as one ack.
        let stored = self.local.write_chunk(envelope.clone(), pool).await?;
        let mut acks: usize = 1;

        // 2. Fan out to peers in parallel. Replication-N: each peer
        //    holds the whole envelope at fragment_index=0.
        if !self.peers.is_empty() {
            let chunk_id = envelope.chunk_id;
            let tenant_id = self.cfg.tenant_id;
            let put_timeout = self.cfg.put_timeout;
            let pool_id = self.cfg.pool.clone();

            let mut futs = Vec::with_capacity(self.peers.len());
            for peer in &self.peers {
                let peer = Arc::clone(peer);
                let env = envelope.clone();
                let pool_id = pool_id.clone();
                futs.push(tokio::spawn(async move {
                    tokio::time::timeout(
                        put_timeout,
                        peer.put_fragment(chunk_id, 0, tenant_id, pool_id, env),
                    )
                    .await
                }));
            }

            for fut in futs {
                match fut.await {
                    Ok(Ok(Ok(_))) => acks += 1,
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(error=%e, "peer PutFragment failed");
                    }
                    Ok(Err(_)) => {
                        tracing::warn!("peer PutFragment timed out");
                    }
                    Err(e) => {
                        tracing::warn!(error=%e, "peer PutFragment join error");
                    }
                }
            }
        }

        // 3. Quorum gate.
        if acks >= self.quorum_required() {
            Ok(stored)
        } else {
            if let Some(m) = self.metrics.as_ref() {
                m.record_quorum_lost();
            }
            Err(ChunkError::QuorumLost {
                acks,
                required: self.quorum_required(),
            })
        }
    }

    async fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError> {
        match self.local.read_chunk(chunk_id).await {
            Ok(env) => return Ok(env),
            Err(ChunkError::NotFound(_)) => {} // fall through to fabric
            Err(other) => return Err(other),
        }

        // Fabric fallback. Replication-N: any 1 fragment_index=0 is
        // sufficient. Walk peers in order; first success wins.
        for peer in &self.peers {
            match tokio::time::timeout(
                self.cfg.get_timeout,
                peer.get_fragment(*chunk_id, 0),
            )
            .await
            {
                Ok(Ok(env)) => return Ok(env),
                Ok(Err(FabricPeerError::NotFound)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error=%e, "peer GetFragment errored, trying next");
                }
                Err(_) => {
                    tracing::warn!("peer GetFragment timed out, trying next");
                }
            }
        }

        Err(ChunkError::NotFound(*chunk_id))
    }

    async fn increment_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        // Refcount is Raft-replicated metadata — local-only here. The
        // Raft state machine ensures every replica converges.
        self.local.increment_refcount(chunk_id).await
    }

    async fn decrement_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        self.local.decrement_refcount(chunk_id).await
    }

    async fn set_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        self.local.set_retention_hold(chunk_id, hold_name).await
    }

    async fn release_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        self.local.release_retention_hold(chunk_id, hold_name).await
    }

    async fn gc(&self) -> u64 {
        self.local.gc().await
    }

    async fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        self.local.refcount(chunk_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
    use kiseki_chunk::store::ChunkStore;
    use kiseki_chunk::SyncBridge;
    use kiseki_common::ids::{ChunkId, OrgId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::envelope::Envelope;

    use super::*;

    fn make_envelope(seed: u8) -> Envelope {
        Envelope {
            chunk_id: ChunkId([seed; 32]),
            ciphertext: vec![seed; 64],
            auth_tag: [0u8; 16],
            nonce: [0u8; 12],
            system_epoch: KeyEpoch(1),
            tenant_epoch: None,
            tenant_wrapped_material: None,
        }
    }

    fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: pool.to_owned(),
            device_class: DeviceClass::NvmeSsd,
            durability: DurabilityStrategy::Replication { copies: 1 },
            devices: vec![],
            capacity_bytes: 1 << 30,
            used_bytes: 0,
        });
        Arc::new(SyncBridge::new(store))
    }

    /// Test peer that records every `PutFragment` + serves `GetFragment`
    /// from its in-memory map. Failure modes can be injected.
    struct MockPeer {
        name: &'static str,
        store: StdMutex<std::collections::HashMap<ChunkId, Envelope>>,
        put_calls: AtomicU64,
        /// If set, every put returns this error instead of storing.
        fail_put: StdMutex<Option<FabricPeerError>>,
        /// If set, every get returns this error.
        fail_get: StdMutex<Option<FabricPeerError>>,
        /// If > 0, sleep this long before responding to put.
        put_delay: StdMutex<Duration>,
        /// If > 0, sleep this long before responding to get.
        get_delay: StdMutex<Duration>,
    }

    impl MockPeer {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                store: StdMutex::new(std::collections::HashMap::new()),
                put_calls: AtomicU64::new(0),
                fail_put: StdMutex::new(None),
                fail_get: StdMutex::new(None),
                put_delay: StdMutex::new(Duration::ZERO),
                get_delay: StdMutex::new(Duration::ZERO),
            })
        }
        fn fail_put(&self, e: FabricPeerError) {
            *self.fail_put.lock().unwrap() = Some(e);
        }
        fn fail_get(&self, e: FabricPeerError) {
            *self.fail_get.lock().unwrap() = Some(e);
        }
        fn delay_put(&self, d: Duration) {
            *self.put_delay.lock().unwrap() = d;
        }
        fn put_count(&self) -> u64 {
            self.put_calls.load(Ordering::SeqCst)
        }
        fn preload(&self, env: Envelope) {
            self.store.lock().unwrap().insert(env.chunk_id, env);
        }
    }

    #[async_trait]
    impl FabricPeer for MockPeer {
        fn name(&self) -> &str {
            self.name
        }
        async fn put_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
            _tenant_id: OrgId,
            _pool_id: String,
            envelope: Envelope,
        ) -> Result<bool, FabricPeerError> {
            self.put_calls.fetch_add(1, Ordering::SeqCst);
            let delay = *self.put_delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(e) = self.fail_put.lock().unwrap().clone() {
                return Err(e);
            }
            self.store.lock().unwrap().insert(chunk_id, envelope);
            Ok(true)
        }
        async fn get_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
        ) -> Result<Envelope, FabricPeerError> {
            let delay = *self.get_delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(e) = self.fail_get.lock().unwrap().clone() {
                return Err(e);
            }
            self.store
                .lock()
                .unwrap()
                .get(&chunk_id)
                .cloned()
                .ok_or(FabricPeerError::NotFound)
        }
        async fn delete_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
            _tenant_id: OrgId,
        ) -> Result<bool, FabricPeerError> {
            Ok(self.store.lock().unwrap().remove(&chunk_id).is_some())
        }
        async fn has_fragment(
            &self,
            chunk_id: ChunkId,
            _fragment_index: u32,
        ) -> Result<bool, FabricPeerError> {
            Ok(self.store.lock().unwrap().contains_key(&chunk_id))
        }
    }

    /// D-6: 1-node cluster (no peers) write succeeds locally and the
    /// quorum gate degenerates to "local is the whole cluster".
    #[tokio::test]
    async fn single_node_write_succeeds_with_no_peers() {
        let local = local_bridge("p");
        let store = ClusteredChunkStore::new(
            Arc::clone(&local),
            vec![],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let env = make_envelope(0x11);
        let stored = store.write_chunk(env, "p").await.expect("write succeeds");
        assert!(stored);
    }

    /// 3-node Replication-3 happy path: every peer receives the
    /// fragment exactly once and the write returns Ok with stored=true.
    #[tokio::test]
    async fn three_node_write_fans_out_to_each_peer_exactly_once() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        let store = ClusteredChunkStore::new(
            Arc::clone(&local),
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let env = make_envelope(0x22);
        let stored = store.write_chunk(env, "p").await.expect("write succeeds");
        assert!(stored);
        assert_eq!(p2.put_count(), 1, "node2 receives exactly one PutFragment");
        assert_eq!(p3.put_count(), 1, "node3 receives exactly one PutFragment");
    }

    /// D-5 quorum: 1 peer down (out of 2) — local + 1 peer = 2-of-3
    /// quorum holds, write succeeds.
    #[tokio::test]
    async fn write_succeeds_with_one_peer_down_at_2of3_quorum() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        p3.fail_put(FabricPeerError::Unavailable("node3 down".into()));
        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let env = make_envelope(0x33);
        let stored = store.write_chunk(env, "p").await.expect("write");
        assert!(stored);
    }

    /// D-5 quorum: both peers down — local alone is 1-of-3, fails
    /// the 2-of-3 quorum gate with `QuorumLost`.
    #[tokio::test]
    async fn write_returns_quorum_lost_when_both_peers_down() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        p2.fail_put(FabricPeerError::Unavailable("down".into()));
        p3.fail_put(FabricPeerError::Unavailable("down".into()));
        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let env = make_envelope(0x44);
        let err = store
            .write_chunk(env, "p")
            .await
            .expect_err("must fail with quorum lost");
        assert!(
            matches!(err, ChunkError::QuorumLost { acks: 1, required: 2 }),
            "got {err:?}"
        );
    }

    /// D-5 quorum: slow peer past the timeout treated as down.
    #[tokio::test(start_paused = true)]
    async fn slow_peer_past_timeout_treated_as_down() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        // p3 takes 30s — way over the 5s default timeout.
        p3.delay_put(Duration::from_secs(30));
        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let env = make_envelope(0x55);
        // local + p2 = 2 acks, p3 times out — quorum holds.
        let stored = store.write_chunk(env, "p").await.expect("write succeeds");
        assert!(stored);
    }

    /// D-10 read-side fabric fallback: chunk is missing locally but
    /// present on a peer (e.g. cross-stream ordering — composition
    /// delta arrived ahead of the fragment write to *this* node).
    #[tokio::test]
    async fn read_falls_back_to_peer_on_local_miss() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        let env = make_envelope(0x66);
        let chunk_id = env.chunk_id;
        p3.preload(env.clone()); // only node3 has it
        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let got = store.read_chunk(&chunk_id).await.expect("read");
        assert_eq!(got.chunk_id, chunk_id);
        assert_eq!(got.ciphertext, env.ciphertext);
    }

    /// Read `NotFound` everywhere → propagate `NotFound` (gateway maps
    /// to `NFS4ERR_DELAY`).
    #[tokio::test]
    async fn read_returns_not_found_when_no_peer_has_chunk() {
        let local = local_bridge("p");
        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let missing = ChunkId([0xEEu8; 32]);
        let err = store.read_chunk(&missing).await.expect_err("not found");
        assert!(matches!(err, ChunkError::NotFound(c) if c == missing));
    }

    /// Read prefers local — does not call peers when the chunk is
    /// stored locally.
    #[tokio::test]
    async fn read_prefers_local_and_does_not_query_peers_on_hit() {
        let local = local_bridge("p");
        let env = make_envelope(0x77);
        let chunk_id = env.chunk_id;
        local.write_chunk(env.clone(), "p").await.expect("seed local");

        let p2 = MockPeer::new("node2");
        let p3 = MockPeer::new("node3");
        // If anyone hits these peers for GetFragment, fail loudly.
        p2.fail_get(FabricPeerError::Unavailable("must not be called".into()));
        p3.fail_get(FabricPeerError::Unavailable("must not be called".into()));

        let store = ClusteredChunkStore::new(
            local,
            vec![Arc::clone(&p2) as Arc<dyn FabricPeer>, Arc::clone(&p3) as Arc<dyn FabricPeer>],
            ClusterCfg::new(OrgId(uuid::Uuid::nil()), "p"),
        );
        let got = store.read_chunk(&chunk_id).await.expect("read");
        assert_eq!(got.chunk_id, chunk_id);
    }
}
