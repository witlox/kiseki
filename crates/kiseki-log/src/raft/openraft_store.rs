//! OpenRaft-backed log store.
//!
//! Wraps a `Raft<LogTypeConfig>` handle for consensus-replicated
//! shard operations. Reads from shared state machine inner, writes
//! go through `client_write()`.
//!
//! Provides async methods matching the `LogOps` trait API. The sync
//! `LogOps` trait cannot be implemented directly because the Raft
//! layer is async, but all equivalent operations are available as
//! async methods on this type.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_raft::{
    FjallRaftLogStore, KisekiNode, KisekiRaftConfig, MemLogStore, StubNetworkFactory,
    TcpNetworkFactory,
};
use openraft::type_config::async_runtime::WatchReceiver;
use openraft::Raft;

use super::state_machine::{ShardSmInner, ShardStateMachine};
use super::types::{LogResponse, LogTypeConfig};
use crate::delta::Delta;
use crate::error::LogError;
use crate::raft_store::LogCommand;
use crate::shard::{ShardInfo, ShardState};
use crate::traits::{AppendDeltaRequest, ReadDeltasRequest};
use kiseki_common::locks::LockOrDie;

type C = LogTypeConfig;

/// OpenRaft-backed log store for a single shard.
///
/// Single-node Raft for now. Writes go through `client_write()`,
/// reads from the shared `ShardSmInner`.
///
/// The state machine stores full delta data, consumer watermarks,
/// and shard metadata — enabling `read_deltas`, `truncate_log`,
/// `compact_shard`, and watermark operations.
pub struct OpenRaftLogStore {
    raft: Raft<C, ShardStateMachine>,
    state: Arc<futures::lock::Mutex<ShardSmInner>>,
    shard_id: ShardId,
    tenant_id: OrgId,
    /// Local Raft node id (ADR-042 §4). Stashed at construction so the
    /// proxy code path can detect a self-forward without dipping
    /// into the openraft metrics watch. See [`Self::node_id`].
    local_node_id: u64,
    /// Inline write rate meter (I-SF7): tracks bytes written in the
    /// current sliding window. When rate exceeds budget, the effective
    /// inline threshold drops to floor.
    inline_rate: std::sync::Mutex<InlineRateMeter>,
}

/// Sliding-window rate meter for inline write throughput (I-SF7).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    dead_code
)]
struct InlineRateMeter {
    /// Bytes written in the current window.
    window_bytes: u64,
    /// Window start time (epoch ms).
    window_start_ms: u64,
    /// Window duration (ms).
    window_ms: u64,
    /// Budget in bytes per second.
    budget_bytes_per_sec: u64,
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
impl InlineRateMeter {
    fn new(budget_mbps: u64) -> Self {
        Self {
            window_bytes: 0,
            window_start_ms: 0,
            window_ms: 10_000, // 10-second sliding window
            budget_bytes_per_sec: budget_mbps * 1024 * 1024,
        }
    }

    /// Record an inline write and return whether the rate is exceeded.
    fn record(&mut self, bytes: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        // Reset window if expired.
        if now.saturating_sub(self.window_start_ms) > self.window_ms {
            self.window_bytes = 0;
            self.window_start_ms = now;
        }

        self.window_bytes += bytes;

        // Check if rate exceeds budget.
        let elapsed_secs = (now.saturating_sub(self.window_start_ms)).max(1) as f64 / 1000.0;
        let rate = self.window_bytes as f64 / elapsed_secs;
        rate > self.budget_bytes_per_sec as f64
    }

    /// Check if the current rate exceeds the budget (without recording).
    fn is_exceeded(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        if now.saturating_sub(self.window_start_ms) > self.window_ms {
            return false; // Window expired, rate is zero.
        }

        let elapsed_secs = (now.saturating_sub(self.window_start_ms)).max(1) as f64 / 1000.0;
        let rate = self.window_bytes as f64 / elapsed_secs;
        rate > self.budget_bytes_per_sec as f64
    }
}

/// Map an openraft `client_write` error into either
/// [`LogError::ForwardToLeader`] (with the leader node id surfaced),
/// [`LogError::LeaderUnavailable`] (no known leader), or
/// [`LogError::Unavailable`] (any other Raft-side failure).
///
/// Used by the `*_with_forwarding`-suffix methods on
/// [`OpenRaftLogStore`] to preserve the openraft `ForwardToLeader`
/// hint for ADR-042 §4 server-side proxy / ADR-008 rev 2 client-side
/// hint paths. The legacy methods (`append_delta` and siblings)
/// collapse `ForwardToLeader` onto `LeaderUnavailable` for
/// backwards compatibility.
fn map_raft_error_with_forwarding(
    err: openraft::errors::RaftError<C, openraft::error::ClientWriteError<C>>,
    shard_id: ShardId,
) -> LogError {
    use openraft::error::ClientWriteError;
    use openraft::errors::RaftError;
    match err {
        RaftError::APIError(ClientWriteError::ForwardToLeader(hint)) => match hint.leader_id {
            Some(leader_u64) => LogError::ForwardToLeader {
                shard_id,
                leader_node_id: kiseki_common::ids::NodeId(leader_u64),
            },
            None => LogError::LeaderUnavailable(shard_id),
        },
        _ => LogError::Unavailable,
    }
}

fn op_to_u8(op: crate::delta::OperationType) -> u8 {
    match op {
        crate::delta::OperationType::Create => 0,
        crate::delta::OperationType::Update => 1,
        crate::delta::OperationType::Delete => 2,
        crate::delta::OperationType::Rename => 3,
        crate::delta::OperationType::SetAttribute => 4,
        crate::delta::OperationType::Finalize => 5,
        crate::delta::OperationType::NamespaceCreate => 6,
        crate::delta::OperationType::MigrateChunkLocations => 7,
    }
}

/// Re-export of the operation-code mapper so sibling modules
/// ([`crate::raft_intent_sink`]) can build `IncorporateItem`s without
/// duplicating the table.
#[must_use]
pub fn op_to_u8_pub(op: crate::delta::OperationType) -> u8 {
    op_to_u8(op)
}

impl OpenRaftLogStore {
    /// Create a Raft log store for a shard. **Does not call
    /// `initialize()`** — handle construction is always non-blocking.
    /// Membership setup is a separate, explicit step:
    /// [`OpenRaftLogStore::initialize_membership`] is the seed-node
    /// call; followers learn membership from the leader via
    /// `AppendEntries` and never need to call it.
    ///
    /// Decoupling these two phases is what unblocks ADR-033 §4
    /// cluster-wide split: the apply hook on every node creates
    /// (and registers) the new shard's Raft group locally without
    /// blocking on votes from peers that haven't yet applied the
    /// same `RecordSplit`. The leader of the new shard then calls
    /// `initialize_membership` once every replica is registered.
    ///
    /// When `peers` is empty, runs in single-node mode with a stub
    /// network. When `peers` contains entries, uses the multiplexed
    /// TCP transport (ADR-041).
    pub async fn new(
        node_id: u64,
        shard_id: ShardId,
        tenant_id: OrgId,
        peers: &BTreeMap<u64, String>,
        data_dir: Option<&std::path::Path>,
        inline_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
    ) -> Result<Self, LogError> {
        let config = KisekiRaftConfig::default_config();
        let mut sm_inner = ShardSmInner::new(shard_id, tenant_id);
        if let Some(store) = inline_store {
            sm_inner = sm_inner.with_inline_store(store);
        }
        let state_inner = Arc::new(futures::lock::Mutex::new(sm_inner));
        let state_machine = ShardStateMachine::new(Arc::clone(&state_inner));

        // Select log store backend: persistent (fjall) or in-memory.
        // Errors are surfaced as `LogError::StoreConstruction(<source>)`
        // so operators see the underlying fjall / openraft failure
        // instead of the prior opaque `Unavailable`. Each call site
        // tags its phase so the message reads e.g.
        // "fjall open at /data/raft/shard-…: <fjall::Error>".
        let raft = if let Some(dir) = data_dir {
            let raft_dir = dir.join("raft");
            std::fs::create_dir_all(&raft_dir).map_err(|e| {
                LogError::StoreConstruction(format!("create_dir_all({}): {e}", raft_dir.display()))
            })?;
            let log_path = raft_dir.join(format!("shard-{}", shard_id.0));
            // #151 (W6) — opt-in fsync coalescing. Off by default;
            // operators enable per-deployment by setting both
            // env vars. The window is a duration in microseconds
            // that a fsync request waits for stragglers; the batch
            // is the max waiters per merged fsync. See the
            // `kiseki_raft::fsync_coalescer` module docs for the
            // tuning trade-off (low values approximate the legacy
            // per-AE-round fsync; high values amortise the fsync
            // tax across more entries at the cost of a small
            // floor under low load).
            let fsync_window_us: Option<u64> = std::env::var("KISEKI_RAFT_FSYNC_WINDOW_US")
                .ok()
                .and_then(|s| s.parse().ok());
            let fsync_max_batch: Option<usize> = std::env::var("KISEKI_RAFT_FSYNC_BATCH")
                .ok()
                .and_then(|s| s.parse().ok());
            let log_store = match (fsync_window_us, fsync_max_batch) {
                (Some(w), Some(b)) if w > 0 && b > 0 => {
                    tracing::info!(
                        shard_id = %shard_id.0,
                        window_us = w,
                        max_batch = b,
                        "FjallRaftLogStore: fsync coalescing ON (#151 / W6)",
                    );
                    FjallRaftLogStore::<C>::open_with_fsync_coalescing(&log_path, w, b).map_err(
                        |e| {
                            LogError::StoreConstruction(format!(
                                "fjall open (coalesced) at {}: {e}",
                                log_path.display()
                            ))
                        },
                    )?
                }
                _ => FjallRaftLogStore::<C>::open(&log_path).map_err(|e| {
                    LogError::StoreConstruction(format!(
                        "fjall open at {}: {e}",
                        log_path.display()
                    ))
                })?,
            };
            if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new(shard_id);
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|e| {
                        LogError::StoreConstruction(format!("Raft::new multi-node: {e}"))
                    })?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|e| {
                        LogError::StoreConstruction(format!("Raft::new single-node: {e}"))
                    })?
            }
        } else {
            let log_store = MemLogStore::<C>::new();
            if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new(shard_id);
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|e| {
                        LogError::StoreConstruction(format!(
                            "Raft::new multi-node (mem-store): {e}"
                        ))
                    })?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|e| {
                        LogError::StoreConstruction(format!(
                            "Raft::new single-node (mem-store): {e}"
                        ))
                    })?
            }
        };

        Ok(Self {
            raft,
            state: state_inner,
            shard_id,
            tenant_id,
            local_node_id: node_id,
            inline_rate: std::sync::Mutex::new(InlineRateMeter::new(10)), // 10 MB/s default
        })
    }

    /// Initialize the Raft cluster membership for this shard. Call
    /// this ONLY on the seed node, ONLY once, and ONLY after every
    /// replica's `OpenRaftLogStore::new` has registered the shard
    /// with the multiplexed transport — otherwise the seed's vote
    /// requests race against peer registration.
    ///
    /// Idempotent against persistent restarts (`already_initialized`
    /// state is checked internally by openraft) and against
    /// repeated calls on the same handle (subsequent calls return
    /// `NotAllowed` which we map to `Ok(())`).
    pub async fn initialize_membership(
        &self,
        peers: &BTreeMap<u64, String>,
    ) -> Result<(), LogError> {
        let members: BTreeMap<u64, KisekiNode> = if peers.len() > 1 {
            peers
                .iter()
                .map(|(id, addr)| (*id, KisekiNode::new(addr)))
                .collect()
        } else {
            // Single-node fallback: same address derivation as `new`.
            let mut m = BTreeMap::new();
            let node_id = peers.keys().copied().next().unwrap_or(1);
            let addr = peers.get(&node_id).map_or("localhost:9201", String::as_str);
            m.insert(node_id, KisekiNode::new(addr));
            m
        };
        match self.raft.initialize(members).await {
            Ok(()) => Ok(()),
            // openraft returns `NotAllowed` when the group is
            // already initialized (persistent restart) — treat as
            // success so the runtime's bootstrap path is idempotent.
            Err(e) => {
                let s = format!("{e}");
                if s.contains("not allowed") || s.contains("already initialized") {
                    Ok(())
                } else {
                    tracing::warn!(error = %s, "initialize_membership failed");
                    Err(LogError::Unavailable)
                }
            }
        }
    }

    /// Check if the inline write rate is currently exceeded (I-SF7).
    ///
    /// Returns `true` if the effective inline threshold should drop
    /// to floor to prevent inline writes from starving Raft.
    #[must_use]
    pub fn inline_rate_exceeded(&self) -> bool {
        self.inline_rate
            .lock()
            .lock_or_die("openraft_store.inline_rate")
            .is_exceeded()
    }

    /// Record an inline write for rate tracking (I-SF7).
    pub fn record_inline_write(&self, bytes: u64) -> bool {
        self.inline_rate
            .lock()
            .lock_or_die("openraft_store.inline_rate")
            .record(bytes)
    }

    /// Get a shareable handle to this shard's `Raft` instance.
    ///
    /// Per ADR-041, the per-node `RaftRpcListener` owns the TCP
    /// listener; each shard registers its `Raft` handle into the
    /// listener's `RegistryHandle` so inbound RPCs route correctly.
    /// This method returns the handle that goes into
    /// `RegistryHandle::register_shard(shard_id, raft_handle)`.
    #[must_use]
    pub fn raft_handle(&self) -> Arc<openraft::Raft<C, ShardStateMachine>> {
        Arc::new(self.raft.clone())
    }

    /// This shard's current Raft **voter** node ids, read from the live
    /// membership metrics (ADR-047 phase 5b). Learners are excluded — only
    /// voters carry the durability quorum the recovery gather counts against.
    /// The set includes this node when it is itself a voter; the caller (the
    /// [`TransportIntentGatherer`](crate::intent_sync)) drops the local id
    /// before fanning out. Empty before membership is initialized.
    #[must_use]
    pub fn voter_ids(&self) -> Vec<u64> {
        self.raft
            .metrics()
            .borrow_watched()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    /// Is this node the **established leader** of this shard's Raft group right
    /// now (ADR-047 `LeaderSink` leadership detection)? Reads
    /// `ServerState::Leader` from the live metrics watch — a cheap, lock-free
    /// borrow, safe to poll on the committer supervisor's tick. A deposed
    /// leader flips to `Candidate`/`Follower` here as soon as openraft sees a
    /// higher term, which is the supervisor's signal to stop draining.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }

    /// The node id this shard's Raft group currently regards as leader, if any
    /// (ADR-047 `LeaderSink` — the `put_intent_and_fan` fan-includes-leader
    /// target). `None` during an election (no committed leader). Read from the
    /// live metrics watch.
    #[must_use]
    pub fn current_leader_id(&self) -> Option<u64> {
        self.raft.metrics().borrow_watched().current_leader
    }

    /// Append a delta through Raft consensus.
    ///
    /// Accepts an `AppendDeltaRequest` (the `LogOps` trait's request type).
    /// Pre-checks maintenance mode and key range before writing.
    /// Returns the assigned sequence number.
    pub async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        // Pre-check state.
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        };

        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// Append a delta through Raft consensus, surfacing the
    /// openraft `ForwardToLeader` hint to the caller (ADR-042 §4).
    ///
    /// Identical to [`Self::append_delta`] in success behavior, but
    /// the error mapping is different:
    ///
    /// | openraft outcome                                                  | this method            | [`Self::append_delta`] |
    /// |---|---|---|
    /// | `Ok(_)`                                                           | `Ok(seq)`              | `Ok(seq)`              |
    /// | `ClientWriteError::ForwardToLeader(hint)` with `Some(leader_id)`  | `Err(ForwardToLeader)` | `Err(LeaderUnavailable)` |
    /// | `ClientWriteError::ForwardToLeader(hint)` with `None`             | `Err(LeaderUnavailable)` | `Err(LeaderUnavailable)` |
    /// | Other Raft errors                                                 | `Err(Unavailable)`     | `Err(Unavailable)`     |
    ///
    /// Callers that opt into the forwarding hint (the native gRPC
    /// server with `KISEKI_NATIVE_PROXY_FALLBACK=on`; the S3
    /// gateway's 307-redirect path; the native client's
    /// topology-cache refresh path) use this method. Existing
    /// callers that don't yet handle `ForwardToLeader` keep using
    /// [`Self::append_delta`] and observe no behavior change.
    ///
    /// # Errors
    /// - [`LogError::MaintenanceMode`] if the shard is in maintenance.
    /// - [`LogError::ForwardToLeader`] if the local replica is a
    ///   follower and openraft knows the leader id.
    /// - [`LogError::LeaderUnavailable`] if no leader is currently known.
    /// - [`LogError::Unavailable`] for any other Raft-side write
    ///   failure.
    pub async fn append_delta_with_forwarding(
        &self,
        req: AppendDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        // Pre-check state. Mirror of `append_delta` — kept inline
        // rather than refactored into a shared helper so the two
        // methods stay easy to compare diff-side-by-side during
        // ADR-042 §4 review.
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        };

        let resp = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|e| map_raft_error_with_forwarding(e, self.shard_id))?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// Append a delta through Raft consensus (raw parameters).
    ///
    /// Lower-level method that accepts raw byte arrays. Prefer
    /// `append_delta` with `AppendDeltaRequest` for type safety.
    pub async fn append_delta_raw(
        &self,
        tenant_id_bytes: [u8; 16],
        operation: u8,
        hashed_key: [u8; 32],
        chunk_refs: Vec<[u8; 32]>,
        payload: Vec<u8>,
        has_inline_data: bool,
    ) -> Result<SequenceNumber, LogError> {
        // Pre-check state.
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes,
            operation,
            hashed_key,
            chunk_refs,
            payload,
            has_inline_data,
        };

        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// Atomic chunk-meta-create + delta-append (Phase 16b D-4).
    /// Submits a single `ChunkAndDelta` Raft proposal so every replica
    /// applies the `cluster_chunk_state` seed and the delta together.
    pub async fn append_chunk_and_delta(
        &self,
        req: AppendDeltaRequest,
        new_chunks: Vec<crate::raft_store::NewChunkMeta>,
    ) -> Result<SequenceNumber, LogError> {
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::ChunkAndDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
            new_chunks,
        };

        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// ADR-047 PART 8 — incorporate a BATCH of async-committed intents into
    /// the Raft log as a single [`LogCommand::IncorporateIntents`] command.
    ///
    /// Replaces the per-intent `append_intent` (Finding U). Each item runs
    /// through the per-item SM gate (recent set + ancient cutoff); items
    /// already in the recent set are no-ops, items below the cutoff are
    /// refused-with-alarm. Atomicity is preserved across the batch — the
    /// whole apply runs under one SM-lock-held block.
    ///
    /// # Errors
    /// [`LogError::MaintenanceMode`] if the shard is draining;
    /// [`LogError::LeaderUnavailable`] if this node is not the leader;
    /// [`LogError::Unavailable`] on any other Raft client-write failure.
    pub async fn append_intents(
        &self,
        items: Vec<crate::raft_store::IncorporateItem>,
    ) -> Result<SequenceNumber, LogError> {
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }
        if items.is_empty() {
            // Nothing to do — return the current tip so callers can rely on
            // a SequenceNumber result.
            return Ok(self.current_tip().await);
        }

        let cmd = LogCommand::IncorporateIntents { items };
        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// ADR-042 §4 — `append_chunk_and_delta` that surfaces
    /// `LogError::ForwardToLeader` instead of `LeaderUnavailable`.
    /// Used by [`crate::traits::LogOps::append_chunk_and_delta_with_forwarding`]
    /// and through it by [`crate::raft_shard_store::RaftShardStore`].
    pub async fn append_chunk_and_delta_with_forwarding(
        &self,
        req: AppendDeltaRequest,
        new_chunks: Vec<crate::raft_store::NewChunkMeta>,
    ) -> Result<SequenceNumber, LogError> {
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::ChunkAndDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
            new_chunks,
        };

        let resp = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|e| map_raft_error_with_forwarding(e, self.shard_id))?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok | LogResponse::DecrementOutcome(_) => Err(LogError::Unavailable),
        }
    }

    /// Bump a chunk's `cluster_chunk_state` refcount (Phase 16b).
    pub async fn increment_chunk_refcount(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        chunk_id: kiseki_common::ids::ChunkId,
    ) -> Result<(), LogError> {
        let cmd = LogCommand::IncrementChunkRefcount {
            tenant_id_bytes: *tenant_id.0.as_bytes(),
            chunk_id: chunk_id.0,
        };
        self.raft
            .client_write(cmd)
            .await
            .map_err(|_| LogError::Unavailable)?;
        Ok(())
    }

    /// Decrement a chunk's `cluster_chunk_state` refcount (Phase 16b).
    /// Phase 16c: returns `true` iff this apply transitioned the entry
    /// to tombstoned (refcount hit 0); the leader uses that signal to
    /// fan `DeleteFragment` out to the placement list.
    pub async fn decrement_chunk_refcount(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        chunk_id: kiseki_common::ids::ChunkId,
    ) -> Result<bool, LogError> {
        let cmd = LogCommand::DecrementChunkRefcount {
            tenant_id_bytes: *tenant_id.0.as_bytes(),
            chunk_id: chunk_id.0,
        };
        let resp = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|_| LogError::Unavailable)?;
        match resp.response() {
            LogResponse::DecrementOutcome(tomb) => Ok(*tomb),
            // Older path / unrelated responses: treat as not-tombstoned;
            // the worst that can happen is a missed fan-out, which the
            // under-replication scrub eventually catches.
            _ => Ok(false),
        }
    }

    /// Phase 16c step 3: read a single `cluster_chunk_state` row.
    pub async fn cluster_chunk_state_get(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        chunk_id: kiseki_common::ids::ChunkId,
    ) -> Option<crate::raft::state_machine::ClusterChunkStateEntry> {
        let inner = self.state.lock().await;
        inner
            .cluster_chunk_state
            .get(&(tenant_id, chunk_id))
            .cloned()
    }

    /// Phase 16c step 3: iterate every `cluster_chunk_state` row.
    pub async fn cluster_chunk_state_iter(
        &self,
    ) -> Vec<(
        kiseki_common::ids::OrgId,
        kiseki_common::ids::ChunkId,
        crate::raft::state_machine::ClusterChunkStateEntry,
    )> {
        let inner = self.state.lock().await;
        inner
            .cluster_chunk_state
            .iter()
            .map(|((t, c), e)| (*t, *c, e.clone()))
            .collect()
    }

    /// Read deltas in `[from, to]` inclusive from the shard.
    ///
    /// For inline deltas (`has_inline_data=true`), reconstructs the
    /// payload from the inline store if the in-memory ciphertext was
    /// cleared by the state machine offload (I-SF5).
    pub async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        if req.from > req.to {
            return Err(LogError::InvalidRange(self.shard_id));
        }

        let inner = self.state.lock().await;
        // `deltas` is appended in strictly increasing sequence order
        // (tip += 1, then push) and the snapshot-install path preserves
        // that order, so it is sorted ascending by `header.sequence`.
        // Binary-search to the range start and stop at `to` instead of
        // scanning the whole vec — O(log N + range), not O(total log).
        // The old O(total) scan ran on every hydrator poll, so hydration
        // degraded as the log grew (the root of #133's ~50 deltas/s) and
        // held the shared SM mutex for the whole scan.
        let start = inner
            .deltas
            .partition_point(|d| d.header.sequence < req.from);
        let deltas: Vec<Delta> = inner.deltas[start..]
            .iter()
            .take_while(|d| d.header.sequence <= req.to)
            .map(|d| {
                // Reconstruct inline payload from store if needed.
                // Key = hashed_key with sequence mixed into last 8 bytes
                // (must match the key used in apply_command).
                if d.header.has_inline_data && d.payload.ciphertext.is_empty() {
                    if let Some(ref store) = inner.inline_store {
                        let mut inline_key = d.header.hashed_key;
                        let seq_bytes = d.header.sequence.0.to_le_bytes();
                        for (i, &b) in seq_bytes.iter().enumerate() {
                            inline_key[24 + i] ^= b;
                        }
                        if let Ok(Some(data)) = store.get(&inline_key) {
                            let mut reconstructed = d.clone();
                            reconstructed.payload.ciphertext = data;
                            return reconstructed;
                        }
                    }
                }
                d.clone()
            })
            .collect();
        Ok(deltas)
    }

    /// Set or clear maintenance mode through Raft consensus.
    pub async fn set_maintenance(&self, enabled: bool) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::SetMaintenance { enabled })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;

        Ok(())
    }

    /// Update the shard's `[range_start, range_end)` key range
    /// through Raft consensus (ADR-033 §4). Used by `split_shard`
    /// (shrinks source range, sets new shard's range) and
    /// `merge_shards` (extends target's range to cover the union).
    ///
    /// # Errors
    /// `LogError::LeaderUnavailable` if the local replica is not
    /// the leader; `LogError::Unavailable` for other write failures.
    pub async fn set_shard_range(
        &self,
        range_start: [u8; 32],
        range_end: [u8; 32],
    ) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::UpdateShardRange {
                range_start,
                range_end,
            })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;
        Ok(())
    }

    /// Transition the shard's lifecycle state through Raft consensus
    /// (ADR-033 §3 / ADR-034 cutover gates). The W4 maintenance
    /// flag is a separate field — use `set_maintenance` for that.
    ///
    /// # Errors
    /// As `set_shard_range`.
    pub async fn set_shard_state(&self, state: ShardState) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::SetShardState {
                state: state.as_u8(),
            })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;
        Ok(())
    }

    /// Replace the shard's `ShardConfig` through Raft consensus.
    /// Auto-split triggers (`max_delta_count`, `max_byte_size`) read
    /// from this — every replica must agree on the thresholds or
    /// they'll fire on different writes.
    ///
    /// # Errors
    /// As `set_shard_range`.
    pub async fn set_shard_config(
        &self,
        config: crate::shard::ShardConfig,
    ) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::SetShardConfig {
                max_delta_count: config.max_delta_count,
                max_byte_size: config.max_byte_size,
                inline_threshold_bytes: config.inline_threshold_bytes,
                inline_floor_bytes: config.inline_floor_bytes,
                inline_ceiling_bytes: config.inline_ceiling_bytes,
            })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;
        Ok(())
    }

    /// Get the current tip sequence number from the state machine.
    pub async fn current_tip(&self) -> SequenceNumber {
        let inner = self.state.lock().await;
        SequenceNumber(inner.tip)
    }

    /// ADR-047 PART 8 §T — snapshot the perspective-seqs currently in the SM's
    /// recent-incorporated set. The supervisor calls this each tick and
    /// per-intent-prunes the local store so only intents *known to be applied
    /// on this replica* are removed.
    pub async fn recent_incorporated_snapshot(
        &self,
    ) -> std::collections::HashSet<kiseki_common::time::HybridLogicalClock> {
        let inner = self.state.lock().await;
        inner.recent_incorporated_snapshot()
    }

    /// ADR-047 PART 8 — the SM's ancient cutoff log-index. Recovery uses this
    /// to filter out re-gathered intents whose intent-store residency is
    /// suspiciously old (Finding Q).
    pub async fn ancient_cutoff_log_index(&self) -> u64 {
        let inner = self.state.lock().await;
        inner.ancient_cutoff_log_index
    }

    /// PART 8 §W — the SM's last-applied Raft log index. Used by the
    /// supervisor's post-promotion wait-for-current: drain does NOT start
    /// until `applied_log_index >= committed_log_index`, so the recent set is
    /// guaranteed to cover the just-promoted leader's incoming log.
    #[must_use]
    pub fn applied_log_index(&self) -> u64 {
        self.raft
            .metrics()
            .borrow_watched()
            .last_applied
            .as_ref()
            .map_or(0, openraft::LogId::index)
    }

    /// PART 8 §W — the SM's last-committed Raft log index. Used as the bar the
    /// applied index must catch up to before draining resumes after promotion.
    #[must_use]
    pub fn committed_log_index(&self) -> u64 {
        self.raft
            .metrics()
            .borrow_watched()
            .committed
            .as_ref()
            .map_or(0, openraft::LogId::index)
    }

    /// Check whether the shard is in maintenance mode.
    pub async fn is_maintenance(&self) -> bool {
        let inner = self.state.lock().await;
        inner.maintenance
    }

    /// Get shard health metadata from the state machine.
    ///
    /// The lowest sequence number still visible in this shard's
    /// delta log. Returns `SequenceNumber(0)` if the delta vec is
    /// empty (no writes yet, or all GC'd). Used by the composition
    /// hydrator's compaction-gap detection per ADR-040 §D6.3
    /// (amended for issue #87) — positive evidence that compaction
    /// has GC'd entries below the returned value.
    pub async fn earliest_visible_seq(&self) -> SequenceNumber {
        let inner = self.state.lock().await;
        inner
            .deltas
            .first()
            .map_or(SequenceNumber(0), |d| d.header.sequence)
    }

    /// Includes Raft leader and membership info from metrics.
    pub async fn shard_health(&self) -> ShardInfo {
        let inner = self.state.lock().await;

        // Read leader from Raft metrics.
        let leader = self
            .raft
            .current_leader()
            .await
            .map(kiseki_common::ids::NodeId);

        // Read membership from Raft metrics.
        let metrics = self.raft.metrics().borrow_watched().clone();
        let raft_members: Vec<kiseki_common::ids::NodeId> = metrics
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| kiseki_common::ids::NodeId(*id))
            .collect();

        ShardInfo {
            shard_id: self.shard_id,
            tenant_id: self.tenant_id,
            raft_members,
            leader,
            tip: SequenceNumber(inner.tip),
            delta_count: inner.delta_count,
            byte_size: inner
                .deltas
                .iter()
                .map(|d| u64::from(d.header.payload_size) + 128)
                .sum(),
            // W4 maintenance overrides the lifecycle state in the
            // health report — it's the operator's "drain in
            // progress" signal and must be visible regardless of
            // whether the shard happens to be Splitting/Merging.
            state: if inner.maintenance {
                ShardState::Maintenance
            } else {
                inner.state
            },
            config: inner.config.clone(),
            range_start: inner.range_start,
            range_end: inner.range_end,
        }
    }

    /// Advance a consumer watermark through Raft consensus.
    pub async fn advance_watermark(
        &self,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::AdvanceWatermark {
                consumer: consumer.to_owned(),
                position: position.0,
            })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;

        Ok(())
    }

    /// Register a consumer watermark (delegates to `advance_watermark`
    /// since the state machine's `advance` handles initial registration).
    pub async fn register_consumer(
        &self,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        self.advance_watermark(consumer, position).await
    }

    /// Truncate deltas below the minimum consumer watermark (GC).
    ///
    /// This is a local operation — GC does not require consensus
    /// because it only removes data that all consumers have already
    /// processed.
    pub async fn truncate_log(&self) -> Result<SequenceNumber, LogError> {
        let mut inner = self.state.lock().await;
        let gc_boundary = inner.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));
        // Delete inline store entries for GC'd inline deltas (I-SF6).
        if let Some(ref store) = inner.inline_store {
            for d in &inner.deltas {
                if d.header.sequence < gc_boundary && d.header.has_inline_data {
                    let mut inline_key = d.header.hashed_key;
                    let seq_bytes = d.header.sequence.0.to_le_bytes();
                    for (i, &b) in seq_bytes.iter().enumerate() {
                        inline_key[24 + i] ^= b;
                    }
                    let _ = store.delete(&inline_key);
                }
            }
        }
        inner.deltas.retain(|d| d.header.sequence >= gc_boundary);
        Ok(gc_boundary)
    }

    /// Compact deltas: keep only the latest delta per `hashed_key`,
    /// remove tombstones below the GC boundary.
    ///
    /// Returns the number of deltas removed.
    pub async fn compact_shard(&self) -> Result<u64, LogError> {
        use std::collections::HashMap;

        let mut inner = self.state.lock().await;
        let before = inner.deltas.len() as u64;
        let gc_boundary = inner.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));

        let mut latest: HashMap<[u8; 32], &Delta> = HashMap::new();
        for delta in &inner.deltas {
            let entry = latest.entry(delta.header.hashed_key).or_insert(delta);
            if delta.header.sequence > entry.header.sequence {
                *entry = delta;
            }
        }

        let surviving: Vec<Delta> = latest
            .into_values()
            .filter(|d| !(d.header.tombstone && d.header.sequence < gc_boundary))
            .cloned()
            .collect();

        // Delete inline store entries for compacted inline deltas (I-SF6).
        if let Some(ref store) = inner.inline_store {
            let surviving_seqs: std::collections::HashSet<u64> =
                surviving.iter().map(|d| d.header.sequence.0).collect();
            for d in &inner.deltas {
                if d.header.has_inline_data && !surviving_seqs.contains(&d.header.sequence.0) {
                    let mut inline_key = d.header.hashed_key;
                    let seq_bytes = d.header.sequence.0.to_le_bytes();
                    for (i, &b) in seq_bytes.iter().enumerate() {
                        inline_key[24 + i] ^= b;
                    }
                    let _ = store.delete(&inline_key);
                }
            }
        }

        let after = surviving.len() as u64;
        inner.deltas = surviving;
        inner.deltas.sort_by_key(|d| d.header.sequence);
        inner.delta_count = after;

        Ok(before.saturating_sub(after))
    }

    /// Get the shard ID this store manages.
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Get the tenant ID this store belongs to.
    #[must_use]
    pub fn tenant_id(&self) -> OrgId {
        self.tenant_id
    }

    /// The raft node id this store's local replica owns. Used by the
    /// native server proxy path's self-forward defense (ADR-042 §4
    /// gate-1 finding C-H2) — the proxy MUST reject
    /// `leader_node_id == self.node_id()` as a stale-Raft self-loop.
    #[must_use]
    pub fn node_id(&self) -> u64 {
        // The raft handle keeps this internally; openraft 0.10 exposes
        // it via the metrics watch. Avoid pulling metrics on the hot
        // path — instead we stash the value at construction time.
        self.local_node_id
    }
}

#[cfg(test)]
mod adr044_tests {
    //! ADR-042 §4 — `ForwardToLeader` extraction unit tests.
    //!
    //! Drives the `map_raft_error_with_forwarding` helper through
    //! every branch of `openraft::error::ClientWriteError` to
    //! validate the mapping invariants documented in the
    //! `append_delta_with_forwarding` rustdoc table.

    use super::{map_raft_error_with_forwarding, C};
    use crate::error::LogError;
    use kiseki_common::ids::{NodeId, ShardId};
    use openraft::error::ClientWriteError;
    use openraft::errors::{Fatal, RaftError};

    fn dummy_shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(
            0x044_0000_0000_0000_0000_0000_0000_0001,
        ))
    }

    #[test]
    fn forward_to_leader_with_known_leader_id_maps_to_forward_variant() {
        let hint = openraft::error::ForwardToLeader::<C>::new(
            7,
            kiseki_raft::KisekiNode::new("127.0.0.1:9100"),
        );
        let err: RaftError<C, ClientWriteError<C>> =
            RaftError::APIError(ClientWriteError::ForwardToLeader(hint));
        let mapped = map_raft_error_with_forwarding(err, dummy_shard());
        match mapped {
            LogError::ForwardToLeader {
                shard_id,
                leader_node_id,
            } => {
                assert_eq!(shard_id, dummy_shard());
                assert_eq!(leader_node_id, NodeId(7));
            }
            other => panic!("expected ForwardToLeader, got {other:?}"),
        }
    }

    #[test]
    fn forward_to_leader_with_unknown_leader_id_falls_back_to_leader_unavailable() {
        let hint = openraft::error::ForwardToLeader::<C>::empty();
        let err: RaftError<C, ClientWriteError<C>> =
            RaftError::APIError(ClientWriteError::ForwardToLeader(hint));
        let mapped = map_raft_error_with_forwarding(err, dummy_shard());
        match mapped {
            LogError::LeaderUnavailable(id) => assert_eq!(id, dummy_shard()),
            other => panic!("expected LeaderUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn raft_fatal_error_maps_to_unavailable() {
        // Any non-ForwardToLeader Raft error collapses to
        // `LogError::Unavailable` — clients retry, no forwarding hint.
        let fatal: Fatal<C> = Fatal::Stopped;
        let err: RaftError<C, ClientWriteError<C>> = RaftError::Fatal(fatal);
        let mapped = map_raft_error_with_forwarding(err, dummy_shard());
        assert!(
            matches!(mapped, LogError::Unavailable),
            "expected LogError::Unavailable, got {mapped:?}"
        );
    }
}
