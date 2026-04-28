//! Async-trait wrapper around the sync [`ChunkOps`] interface.
//!
//! Phase 16a — D-7. The cluster fabric layer is async (gRPC), but
//! every existing local store implements the sync [`ChunkOps`]
//! trait. Calling sync methods directly from a tokio worker would
//! risk deadlocks (the inner store may take time to write to disk).
//! Instead, [`SyncBridge`] owns the inner store behind a
//! `tokio::sync::Mutex` and dispatches each sync call onto a
//! `spawn_blocking` thread, so the async caller never blocks the
//! tokio reactor.
//!
//! `ClusteredChunkStore` (step 4, `kiseki-chunk-cluster`) will
//! implement [`AsyncChunkOps`] directly. Existing single-node
//! deployments wrap their `ChunkStore` / `PersistentChunkStore` in
//! a [`SyncBridge`] when wiring into the gateway runtime.

use std::sync::Arc;

use async_trait::async_trait;
use kiseki_common::ids::ChunkId;
use kiseki_crypto::envelope::Envelope;
use tokio::sync::Mutex;

use crate::error::ChunkError;
use crate::store::ChunkOps;

/// Async parallel of [`ChunkOps`]. Methods take `&self` (interior
/// mutability is the implementer's concern — [`SyncBridge`] uses
/// a `tokio::sync::Mutex`; `ClusteredChunkStore` uses internal
/// state plus per-peer fabric calls).
#[async_trait]
pub trait AsyncChunkOps: Send + Sync {
    /// Write a chunk. Returns `true` if newly stored, `false` on
    /// dedup hit (refcount bumped).
    async fn write_chunk(&self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError>;

    /// Read a chunk by ID.
    async fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError>;

    /// Increment refcount for an existing chunk.
    async fn increment_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Decrement refcount. Returns the new refcount.
    async fn decrement_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;

    /// Set a retention hold on a chunk (I-C2b).
    async fn set_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError>;

    /// Release a retention hold.
    async fn release_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError>;

    /// Run GC: delete chunks with refcount=0 and no retention holds.
    /// Returns the number of chunks deleted.
    async fn gc(&self) -> u64;

    /// Get the refcount for a chunk.
    async fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError>;
}

/// Adapter that exposes any sync [`ChunkOps`] as [`AsyncChunkOps`]
/// without blocking the tokio reactor.
///
/// The inner store sits behind an `Arc<Mutex<T>>` so each async
/// method can lock-then-spawn-blocking-then-release without holding
/// the lock across `.await` points (the lock is only held by the
/// blocking task). The `Arc` lets the lock guard move into the
/// `spawn_blocking` closure cheaply.
pub struct SyncBridge<T: ChunkOps + Send + 'static> {
    inner: Arc<Mutex<T>>,
}

impl<T: ChunkOps + Send + 'static> SyncBridge<T> {
    /// Wrap a sync chunk store as an async one.
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Borrow the inner mutex (test helper / integration glue).
    #[must_use]
    pub fn inner(&self) -> Arc<Mutex<T>> {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl<T: ChunkOps + Send + 'static> AsyncChunkOps for SyncBridge<T> {
    async fn write_chunk(&self, envelope: Envelope, pool: &str) -> Result<bool, ChunkError> {
        let inner = Arc::clone(&self.inner);
        let pool = pool.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.write_chunk(envelope, &pool)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Envelope, ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_lock();
            guard.read_chunk(&chunk_id)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn increment_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.increment_refcount(&chunk_id)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn decrement_refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.decrement_refcount(&chunk_id)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn set_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        let hold_name = hold_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.set_retention_hold(&chunk_id, &hold_name)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn release_retention_hold(
        &self,
        chunk_id: &ChunkId,
        hold_name: &str,
    ) -> Result<(), ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        let hold_name = hold_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.release_retention_hold(&chunk_id, &hold_name)
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn gc(&self) -> u64 {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            guard.gc()
        })
        .await
        .expect("spawn_blocking panicked")
    }

    async fn refcount(&self, chunk_id: &ChunkId) -> Result<u64, ChunkError> {
        let inner = Arc::clone(&self.inner);
        let chunk_id = *chunk_id;
        tokio::task::spawn_blocking(move || {
            let guard = inner.blocking_lock();
            guard.refcount(&chunk_id)
        })
        .await
        .expect("spawn_blocking panicked")
    }
}

#[cfg(test)]
mod tests {
    use kiseki_common::ids::ChunkId;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_crypto::envelope::Envelope;

    use super::{AsyncChunkOps, SyncBridge};
    use crate::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
    use crate::store::{ChunkOps, ChunkStore};

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

    fn store_with_pool(name: &str) -> ChunkStore {
        let mut store = ChunkStore::new();
        store.add_pool(AffinityPool {
            name: name.to_owned(),
            device_class: DeviceClass::NvmeSsd,
            durability: DurabilityStrategy::Replication { copies: 1 },
            devices: vec![],
            capacity_bytes: 1 << 30,
            used_bytes: 0,
        });
        store
    }

    /// `SyncBridge` is a real bridge: writing through the async API
    /// must produce a chunk that the *inner* sync store sees.
    #[tokio::test]
    async fn write_through_async_persists_to_inner_sync_store() {
        let bridge = SyncBridge::new(store_with_pool("p"));
        let env = make_envelope(0xAA);
        let chunk_id = env.chunk_id;

        let stored = bridge
            .write_chunk(env, "p")
            .await
            .expect("write succeeds");
        assert!(stored, "first write must report newly stored");

        // Inspect via inner mutex — the sync store must agree.
        let inner = bridge.inner();
        let guard = inner.lock().await;
        assert_eq!(
            guard.refcount(&chunk_id).expect("chunk visible"),
            1,
            "the very same chunk must be visible through the sync API"
        );
    }

    /// Round-trip through async: write then read returns the same bytes.
    #[tokio::test]
    async fn async_write_then_async_read_returns_same_envelope() {
        let bridge = SyncBridge::new(store_with_pool("p"));
        let env = make_envelope(0x42);
        let chunk_id = env.chunk_id;
        let original_bytes = env.ciphertext.clone();

        bridge.write_chunk(env, "p").await.expect("write");
        let got = bridge.read_chunk(&chunk_id).await.expect("read");

        assert_eq!(got.chunk_id, chunk_id);
        assert_eq!(got.ciphertext, original_bytes);
    }

    /// Concurrent async callers from multiple tokio tasks must not
    /// deadlock — the whole point of `spawn_blocking` is that the
    /// reactor stays free. This is a smoke test: 16 parallel writes
    /// of distinct chunks must all complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_async_callers_do_not_deadlock() {
        use std::sync::Arc;

        let bridge = Arc::new(SyncBridge::new(store_with_pool("p")));
        let mut handles = Vec::new();
        for i in 0..16u8 {
            let bridge = Arc::clone(&bridge);
            handles.push(tokio::spawn(async move {
                bridge
                    .write_chunk(make_envelope(i), "p")
                    .await
                    .expect("write")
            }));
        }
        for h in handles {
            assert!(h.await.expect("task joined"), "16 distinct chunks");
        }
    }

    /// Refcount semantics survive the bridge: two writes of the same
    /// `chunk_id` → first stored=true, second stored=false; refcount=2.
    #[tokio::test]
    async fn dedup_through_bridge_bumps_refcount() {
        let bridge = SyncBridge::new(store_with_pool("p"));
        let env1 = make_envelope(0x77);
        let env2 = make_envelope(0x77); // same seed → same chunk_id
        let chunk_id = env1.chunk_id;

        let first = bridge.write_chunk(env1, "p").await.expect("write1");
        let second = bridge.write_chunk(env2, "p").await.expect("write2");

        assert!(first, "first write is new");
        assert!(!second, "second write is dedup");
        assert_eq!(bridge.refcount(&chunk_id).await.expect("rc"), 2);
    }
}
