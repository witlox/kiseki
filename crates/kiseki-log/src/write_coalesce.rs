//! ADR-046 (W1) — per-shard write-coalescing queue.
//!
//! Batches independent `ChunkAndDelta` writes arriving within a small
//! window into ONE `BatchChunkAndDelta` Raft entry, amortizing the
//! consensus round across the batch (the small-object write lever — #126).
//! Lives in `kiseki-log` (not the gateway) because forwarded writes (#114)
//! reach the leader via `LogService.AppendChunkAndDelta` and converge with
//! local writes at `RaftShardStore` — so coalescing must happen there to
//! catch both paths.
//!
//! Emission is gated (`KISEKI_WRITE_COALESCE`, default off) — see ADR-046
//! rev-2 C1 (a new `LogCommand` variant is a durable log-format change;
//! every replica must decode it before any leader emits one). Idempotency
//! stays upstream (gateway-side, rev-2 H2); the queue is a pure batcher.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kiseki_common::ids::{SequenceNumber, ShardId};
use kiseki_common::locks::LockOrDie;
use tokio::sync::{mpsc, oneshot};

use crate::error::LogError;
use crate::traits::{AppendChunkAndDeltaRequest, LogOps};

/// The narrow append surface the coalescer needs — one method, so tests
/// can supply a trivial mock instead of a full `LogOps`. Blanket-impl'd for
/// every `LogOps` (the Raft-backed store's override is the real coalescing
/// commit; other impls get the un-coalesced default — see ADR-046 step B).
#[async_trait]
pub trait BatchAppend: Send + Sync {
    /// Commit N `ChunkAndDelta` proposals (all on one shard); returns one
    /// sequence number per item, in order.
    async fn append_batch(
        &self,
        reqs: Vec<AppendChunkAndDeltaRequest>,
    ) -> Result<Vec<SequenceNumber>, LogError>;
}

#[async_trait]
impl<T: LogOps + ?Sized> BatchAppend for T {
    async fn append_batch(
        &self,
        reqs: Vec<AppendChunkAndDeltaRequest>,
    ) -> Result<Vec<SequenceNumber>, LogError> {
        self.append_batch_chunk_and_delta(reqs).await
    }
}

/// Reconstruct a `LogError` for per-waiter fan-out (it isn't `Clone`).
/// CRITICAL: the per-shard, leader-routing variants — especially
/// `ForwardToLeader` — MUST survive so a write submitted on a follower
/// still surfaces the forward hint and the gateway re-issues to the leader
/// (#114). Everything else collapses to a retryable `Unavailable`.
fn fan_error(e: &LogError) -> LogError {
    match e {
        LogError::ForwardToLeader {
            shard_id,
            leader_node_id,
        } => LogError::ForwardToLeader {
            shard_id: *shard_id,
            leader_node_id: *leader_node_id,
        },
        LogError::LeaderUnavailable(s) => LogError::LeaderUnavailable(*s),
        LogError::MaintenanceMode(s) => LogError::MaintenanceMode(*s),
        LogError::ShardSplitting(s) => LogError::ShardSplitting(*s),
        LogError::QuorumLost(s) => LogError::QuorumLost(*s),
        LogError::ShardNotFound(s) => LogError::ShardNotFound(*s),
        _ => LogError::Unavailable,
    }
}

/// Coalescing tunables. `from_env` reads `KISEKI_WRITE_COALESCE_*`.
#[derive(Clone, Debug)]
pub struct CoalesceConfig {
    /// Flush once this many items are queued.
    pub max_batch: usize,
    /// Flush once accumulated payload bytes reach this (keeps the single
    /// `AppendEntries`/log record under the transport max-message — rev-2 H3).
    pub max_batch_bytes: usize,
    /// Max time the first queued item waits for the batch to fill.
    pub flush_interval: Duration,
    /// Bounded per-shard queue capacity (backpressure — rev-2 M1).
    pub queue_depth: usize,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            max_batch: 64,
            max_batch_bytes: 8 * 1024 * 1024,
            flush_interval: Duration::from_micros(500),
            queue_depth: 1024,
        }
    }
}

impl CoalesceConfig {
    /// Read overrides from the environment, falling back to [`Default`].
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            max_batch: env_usize("KISEKI_WRITE_COALESCE_MAX_BATCH", d.max_batch).max(1),
            max_batch_bytes: env_usize("KISEKI_WRITE_COALESCE_MAX_BATCH_BYTES", d.max_batch_bytes)
                .max(1),
            flush_interval: Duration::from_micros(env_usize(
                "KISEKI_WRITE_COALESCE_FLUSH_US",
                usize::try_from(d.flush_interval.as_micros()).unwrap_or(usize::MAX),
            ) as u64),
            queue_depth: env_usize("KISEKI_WRITE_COALESCE_QUEUE_DEPTH", d.queue_depth).max(1),
        }
    }

    /// Whether emission is enabled (`KISEKI_WRITE_COALESCE` truthy). Default
    /// OFF — the mixed-version safety gate (rev-2 C1). Operators enable it
    /// only after every node runs a binary that decodes `BatchChunkAndDelta`.
    #[must_use]
    pub fn enabled() -> bool {
        std::env::var("KISEKI_WRITE_COALESCE").is_ok_and(|v| {
            let v = v.to_ascii_lowercase();
            v == "1" || v == "on" || v == "true" || v == "yes"
        })
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

/// One queued write plus its result channel.
struct Pending {
    req: AppendChunkAndDeltaRequest,
    bytes: usize,
    tx: oneshot::Sender<Result<SequenceNumber, LogError>>,
}

/// Per-shard write coalescer (ADR-046 W1). Generic over the append surface
/// so it's testable with a mock; in production `L` is the Raft-backed store
/// adapter that commits one `BatchChunkAndDelta` per flush.
pub struct WriteCoalescer<L: BatchAppend + ?Sized + 'static> {
    log: Arc<L>,
    cfg: CoalesceConfig,
    /// One bounded sender + flush task per shard, created lazily.
    shards: Mutex<HashMap<ShardId, mpsc::Sender<Pending>>>,
}

impl<L: BatchAppend + ?Sized + 'static> WriteCoalescer<L> {
    /// Build a coalescer over the append surface `log` with `cfg`. Per-shard
    /// flush tasks are spawned lazily on first write to each shard.
    #[must_use]
    pub fn new(log: Arc<L>, cfg: CoalesceConfig) -> Arc<Self> {
        Arc::new(Self {
            log,
            cfg,
            shards: Mutex::new(HashMap::new()),
        })
    }

    /// Submit a write. Resolves with the delta's sequence number once the
    /// batch it lands in commits, or with the (reconstructed) commit error —
    /// `ForwardToLeader` is preserved so the gateway re-routes (#114).
    pub async fn submit(
        self: &Arc<Self>,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let shard_id = req.delta.shard_id;
        let bytes = req.delta.payload.len();
        let (tx, rx) = oneshot::channel();
        let sender = self.sender_for(shard_id);
        if sender.send(Pending { req, bytes, tx }).await.is_err() {
            return Err(LogError::Unavailable);
        }
        rx.await.unwrap_or(Err(LogError::Unavailable))
    }

    fn sender_for(self: &Arc<Self>, shard_id: ShardId) -> mpsc::Sender<Pending> {
        let mut shards = self.shards.lock().lock_or_die("write_coalesce.shards");
        if let Some(s) = shards.get(&shard_id) {
            return s.clone();
        }
        let (tx, rx) = mpsc::channel(self.cfg.queue_depth);
        let this = Arc::clone(self);
        tokio::spawn(this.flush_loop(shard_id, rx));
        shards.insert(shard_id, tx.clone());
        tx
    }

    async fn flush_loop(self: Arc<Self>, shard_id: ShardId, mut rx: mpsc::Receiver<Pending>) {
        while let Some(first) = rx.recv().await {
            let mut batch_bytes = first.bytes;
            let mut batch = vec![first];
            loop {
                if batch.len() >= self.cfg.max_batch || batch_bytes >= self.cfg.max_batch_bytes {
                    break;
                }
                match rx.try_recv() {
                    Ok(p) => {
                        batch_bytes += p.bytes;
                        batch.push(p);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {
                        // rev-2 L1: a lone write doesn't wait the window;
                        // only a forming batch (depth ≥ 2) does.
                        if batch.len() < 2 {
                            break;
                        }
                        match tokio::time::timeout(self.cfg.flush_interval, rx.recv()).await {
                            Ok(Some(p)) => {
                                batch_bytes += p.bytes;
                                batch.push(p);
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
            self.flush(shard_id, batch).await;
        }
    }

    async fn flush(&self, shard_id: ShardId, batch: Vec<Pending>) {
        let batch_size = batch.len();
        let mut reqs = Vec::with_capacity(batch_size);
        let mut txs = Vec::with_capacity(batch_size);
        for p in batch {
            reqs.push(p.req);
            txs.push(p.tx);
        }
        match self.log.append_batch(reqs).await {
            Ok(seqs) if seqs.len() == txs.len() => {
                for (tx, seq) in txs.into_iter().zip(seqs) {
                    let _ = tx.send(Ok(seq));
                }
            }
            Ok(seqs) => {
                tracing::error!(
                    shard = %shard_id.0,
                    got = seqs.len(),
                    want = txs.len(),
                    "write-coalesce: BatchAppended count mismatch — failing batch",
                );
                for tx in txs {
                    let _ = tx.send(Err(LogError::Unavailable));
                }
            }
            Err(e) => {
                // Whole-batch failure (leader change, maintenance, …).
                // Reconstruct the error per waiter so ForwardToLeader
                // survives and the gateway re-routes (rev-2 M2/M3).
                tracing::warn!(
                    shard = %shard_id.0,
                    error = %e,
                    n = txs.len(),
                    "write-coalesce: batch commit failed; fanning error to waiters",
                );
                for tx in txs {
                    let _ = tx.send(Err(fan_error(&e)));
                }
            }
        }
        // ADR-046 rev-2 L2: batch fan-out is trace-level for now (the
        // throughput lift is measured via the existing `raft_commit`
        // put-phase histogram). Promote to a registered `LogMetrics`
        // histogram once the coalescer is plumbed with the metrics handle.
        tracing::trace!(shard = %shard_id.0, batch_size, "write-coalesce: flushed batch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::OperationType;
    use crate::traits::AppendDeltaRequest;
    use kiseki_common::ids::{NodeId, OrgId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Mock append surface: records each batch's size and assigns monotonic
    /// sequence numbers (one per item, like the real apply).
    struct MockLog {
        next_seq: AtomicU64,
        batch_sizes: Mutex<Vec<usize>>,
        err: Option<fn() -> LogError>,
    }

    impl MockLog {
        fn ok() -> Self {
            Self {
                next_seq: AtomicU64::new(1),
                batch_sizes: Mutex::new(Vec::new()),
                err: None,
            }
        }
        fn failing(f: fn() -> LogError) -> Self {
            Self {
                next_seq: AtomicU64::new(1),
                batch_sizes: Mutex::new(Vec::new()),
                err: Some(f),
            }
        }
    }

    #[async_trait]
    impl BatchAppend for MockLog {
        async fn append_batch(
            &self,
            reqs: Vec<AppendChunkAndDeltaRequest>,
        ) -> Result<Vec<SequenceNumber>, LogError> {
            if let Some(f) = self.err {
                return Err(f());
            }
            self.batch_sizes
                .lock()
                .lock_or_die("mock.batch_sizes")
                .push(reqs.len());
            Ok(reqs
                .iter()
                .map(|_| SequenceNumber(self.next_seq.fetch_add(1, Ordering::SeqCst)))
                .collect())
        }
    }

    fn req(shard: ShardId, key: u8) -> AppendChunkAndDeltaRequest {
        AppendChunkAndDeltaRequest {
            delta: AppendDeltaRequest {
                shard_id: shard,
                tenant_id: OrgId(uuid::Uuid::from_u128(1)),
                operation: OperationType::Create,
                timestamp: DeltaTimestamp {
                    hlc: HybridLogicalClock {
                        physical_ms: 0,
                        logical: 0,
                        node_id: NodeId(0),
                    },
                    wall: WallTime {
                        millis_since_epoch: 0,
                        timezone: "UTC".into(),
                    },
                    quality: ClockQuality::Ntp,
                },
                hashed_key: [key; 32],
                chunk_refs: vec![],
                payload: vec![0xab; 32],
                has_inline_data: false,
            },
            new_chunks: vec![],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_write_flushes_immediately() {
        let mock = Arc::new(MockLog::ok());
        let q = WriteCoalescer::new(Arc::clone(&mock), CoalesceConfig::default());
        let shard = ShardId(uuid::Uuid::from_u128(7));
        let seq = q.submit(req(shard, 1)).await.expect("submit");
        assert_eq!(seq.0, 1);
        assert_eq!(
            *mock.batch_sizes.lock().lock_or_die("t"),
            vec![1],
            "lone write → one 1-item batch"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writes_coalesce_and_each_gets_a_seq() {
        let mock = Arc::new(MockLog::ok());
        let cfg = CoalesceConfig {
            flush_interval: Duration::from_millis(50),
            ..CoalesceConfig::default()
        };
        let q = WriteCoalescer::new(Arc::clone(&mock), cfg);
        let shard = ShardId(uuid::Uuid::from_u128(8));

        let mut handles = Vec::new();
        for i in 0..16u8 {
            let q = Arc::clone(&q);
            handles.push(tokio::spawn(async move { q.submit(req(shard, i)).await }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap().expect("submit"));
        }
        let mut got: Vec<u64> = seqs.iter().map(|s| s.0).collect();
        got.sort_unstable();
        assert_eq!(got, (1..=16).collect::<Vec<_>>());

        let sizes = mock.batch_sizes.lock().lock_or_die("t").clone();
        assert_eq!(
            sizes.iter().sum::<usize>(),
            16,
            "every write committed once"
        );
        assert!(
            sizes.len() < 16,
            "16 concurrent writes should coalesce into <16 batches, got {sizes:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn max_batch_caps_batch_size() {
        let mock = Arc::new(MockLog::ok());
        let cfg = CoalesceConfig {
            max_batch: 4,
            flush_interval: Duration::from_millis(50),
            ..CoalesceConfig::default()
        };
        let q = WriteCoalescer::new(Arc::clone(&mock), cfg);
        let shard = ShardId(uuid::Uuid::from_u128(9));
        let mut handles = Vec::new();
        for i in 0..12u8 {
            let q = Arc::clone(&q);
            handles.push(tokio::spawn(async move { q.submit(req(shard, i)).await }));
        }
        for h in handles {
            h.await.unwrap().expect("submit");
        }
        let sizes = mock.batch_sizes.lock().lock_or_die("t").clone();
        assert!(
            sizes.iter().all(|&n| n <= 4),
            "no batch may exceed max_batch=4, got {sizes:?}"
        );
        assert_eq!(sizes.iter().sum::<usize>(), 12);
    }

    /// A commit failure fans the error to every waiter, and a
    /// `ForwardToLeader` survives so the gateway re-routes (rev-2 M3).
    #[tokio::test(flavor = "multi_thread")]
    async fn commit_failure_preserves_forward_to_leader() {
        let mock = Arc::new(MockLog::failing(|| LogError::ForwardToLeader {
            shard_id: ShardId(uuid::Uuid::from_u128(10)),
            leader_node_id: NodeId(42),
        }));
        let q = WriteCoalescer::new(Arc::clone(&mock), CoalesceConfig::default());
        let shard = ShardId(uuid::Uuid::from_u128(10));
        match q.submit(req(shard, 1)).await {
            Err(LogError::ForwardToLeader { leader_node_id, .. }) => {
                assert_eq!(
                    leader_node_id,
                    NodeId(42),
                    "forward hint must survive fan-out"
                );
            }
            other => panic!("expected ForwardToLeader, got {other:?}"),
        }
    }
}
