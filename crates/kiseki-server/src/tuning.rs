//! Cluster-wide tuning parameters (ADR-025 §"Tuning parameters" —
//! cluster level only; per-pool, per-tenant, per-workload tuning
//! lives elsewhere).
//!
//! Backs `StorageAdminService.GetTuningParams` /
//! `SetTuningParams`. The 8 parameters here govern long-running
//! background activity (compaction, GC, scrub, rebalance) plus a
//! few hot-path knobs (inline threshold, snapshot interval).
//!
//! ## State model
//!
//! [`TuningParams`] is an in-memory snapshot held behind a
//! `tokio::sync::RwLock` inside [`TuningStore`]. Reads are
//! lock-cheap; writes go through `set()` which:
//!
//! 1. Validates every field via [`TuningParams::validate`] —
//!    `InvalidArgument` on any out-of-range value, BEFORE touching
//!    the backing store, so a partial update never lands.
//! 2. Persists the new snapshot via the [`TuningPersistence`]
//!    trait. The default in-process store no-ops; the redb-backed
//!    impl writes a single postcard-encoded row to the
//!    `tuning_params` table so a server restart rehydrates the
//!    last-set values.
//! 3. Swaps the in-memory snapshot.
//!
//! ## Raft replication (deferred to W5)
//!
//! Today `set()` lands the value on this node only. W5 will wrap
//! `set()` in a `TuningParamsSet` Raft delta on the cluster
//! control shard so followers converge. The `TuningStore` API
//! is shaped to support that without churn — followers will call
//! the same `set()` from their hydrator step.
//!
//! ## Live hooks (W3 minimum, expand incrementally)
//!
//! Every tuning param exists to influence a subsystem's behavior.
//! The `TuningStore::subscribe()` channel hands subsystems a
//! `watch::Receiver<TuningParams>` so they can react to changes
//! without polling. W3 wires `raft_snapshot_interval` (consumed
//! by the next snapshot) and `scrub_interval_h` (consumed by the
//! scrub scheduler on its next tick). Other params are stored
//! and observable via the API but their hooks land in W4/W5
//! alongside their owning subsystems.

use std::sync::Arc;

use kiseki_proto::v1 as pb;
use tokio::sync::{watch, RwLock};
use tonic::Status;

/// All 8 cluster-wide tuning parameters from ADR-025. Defaults
/// match the proto comments verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TuningParams {
    /// Background compaction throughput cap (MB/s). 10..=1000.
    pub compaction_rate_mb_s: u32,
    /// How often GC scans for reclaimable chunks (seconds). 60..=3600.
    pub gc_interval_s: u32,
    /// Background rebalance/evacuation throughput (MB/s). 0..=500.
    pub rebalance_rate_mb_s: u32,
    /// How often integrity scrub runs (hours). 24..=720.
    pub scrub_interval_h: u32,
    /// Parallel EC repair jobs. 1..=32.
    pub max_concurrent_repairs: u32,
    /// View materialization poll interval (ms). 10..=1000.
    pub stream_proc_poll_ms: u32,
    /// Below this size, data is inlined in the delta (bytes). 512..=65536.
    pub inline_threshold_bytes: u32,
    /// Entries between Raft snapshots. 1000..=100000.
    pub raft_snapshot_interval: u32,
}

impl Default for TuningParams {
    /// ADR-025 §"Cluster-wide tuning" defaults — matched 1:1 with
    /// the proto comments so a fresh cluster boots into the
    /// documented baseline.
    fn default() -> Self {
        Self {
            compaction_rate_mb_s: 100,
            gc_interval_s: 300,
            rebalance_rate_mb_s: 50,
            scrub_interval_h: 168,
            max_concurrent_repairs: 4,
            stream_proc_poll_ms: 100,
            inline_threshold_bytes: 4096,
            raft_snapshot_interval: 10_000,
        }
    }
}

impl TuningParams {
    /// Bounds enforcement. Returns [`tonic::Status::invalid_argument`]
    /// naming the first out-of-range field. ADR-025 §"Cluster-wide
    /// tuning" defines every range; the table is mirrored here so a
    /// reader doesn't have to chase the spec.
    pub fn validate(&self) -> Result<(), Status> {
        check_range("compaction_rate_mb_s", self.compaction_rate_mb_s, 10, 1000)?;
        check_range("gc_interval_s", self.gc_interval_s, 60, 3600)?;
        check_range("rebalance_rate_mb_s", self.rebalance_rate_mb_s, 0, 500)?;
        check_range("scrub_interval_h", self.scrub_interval_h, 24, 720)?;
        check_range("max_concurrent_repairs", self.max_concurrent_repairs, 1, 32)?;
        check_range("stream_proc_poll_ms", self.stream_proc_poll_ms, 10, 1000)?;
        check_range(
            "inline_threshold_bytes",
            self.inline_threshold_bytes,
            512,
            65_536,
        )?;
        check_range(
            "raft_snapshot_interval",
            self.raft_snapshot_interval,
            1000,
            100_000,
        )?;
        Ok(())
    }

    /// Convert to the wire type. Field ordering matches the proto.
    #[must_use]
    pub fn to_proto(self) -> pb::TuningParams {
        pb::TuningParams {
            compaction_rate_mb_s: self.compaction_rate_mb_s,
            gc_interval_s: self.gc_interval_s,
            rebalance_rate_mb_s: self.rebalance_rate_mb_s,
            scrub_interval_h: self.scrub_interval_h,
            max_concurrent_repairs: self.max_concurrent_repairs,
            stream_proc_poll_ms: self.stream_proc_poll_ms,
            inline_threshold_bytes: self.inline_threshold_bytes,
            raft_snapshot_interval: self.raft_snapshot_interval,
        }
    }

    /// Convert from the wire type. Caller must run [`Self::validate`]
    /// after — `from_proto` is infallible (it cannot reject input
    /// at all because protobuf fields default to 0, which is out
    /// of range for almost every param). Validation is a separate
    /// step so the call sites can choose between strict (RPC) and
    /// lenient (rehydrate-then-clamp) behavior.
    #[must_use]
    pub fn from_proto(p: &pb::TuningParams) -> Self {
        Self {
            compaction_rate_mb_s: p.compaction_rate_mb_s,
            gc_interval_s: p.gc_interval_s,
            rebalance_rate_mb_s: p.rebalance_rate_mb_s,
            scrub_interval_h: p.scrub_interval_h,
            max_concurrent_repairs: p.max_concurrent_repairs,
            stream_proc_poll_ms: p.stream_proc_poll_ms,
            inline_threshold_bytes: p.inline_threshold_bytes,
            raft_snapshot_interval: p.raft_snapshot_interval,
        }
    }
}

fn check_range(field: &str, value: u32, lo: u32, hi: u32) -> Result<(), Status> {
    if value < lo || value > hi {
        return Err(Status::invalid_argument(format!(
            "{field} = {value} out of range [{lo}, {hi}]",
        )));
    }
    Ok(())
}

/// Backing store for [`TuningParams`]. The default impl is a
/// no-op (in-memory only); the redb-backed impl persists across
/// restarts. Trait so tests can swap stubs.
pub trait TuningPersistence: Send + Sync + std::fmt::Debug {
    /// Write the snapshot durably. Called inside `TuningStore::set`
    /// AFTER bounds validation, BEFORE the in-memory swap.
    fn persist(&self, params: &TuningParams) -> Result<(), TuningStoreError>;

    /// Load the most-recently-persisted snapshot, or `None` for a
    /// fresh deployment. Called once at server boot.
    fn load(&self) -> Result<Option<TuningParams>, TuningStoreError>;
}

/// Errors that can occur in [`TuningPersistence`] impls. Mapped to
/// `Status::internal` at the RPC boundary — operators don't need
/// to distinguish redb error variants from each other.
#[derive(Debug, thiserror::Error)]
pub enum TuningStoreError {
    /// Redb open / commit / read I/O failure.
    #[error("redb: {0}")]
    Redb(String),
    /// Postcard serialize/deserialize failure.
    #[error("decode: {0}")]
    Decode(String),
}

/// In-memory persistence — discards on restart. Used in tests and
/// in single-node deployments without `KISEKI_DATA_DIR`.
#[derive(Debug, Default)]
pub struct InMemoryTuningPersistence;

impl TuningPersistence for InMemoryTuningPersistence {
    fn persist(&self, _params: &TuningParams) -> Result<(), TuningStoreError> {
        Ok(())
    }

    fn load(&self) -> Result<Option<TuningParams>, TuningStoreError> {
        Ok(None)
    }
}

/// Shared cluster-wide tuning params store.
///
/// Cloning [`TuningStore`] clones the `Arc`s so all clones see the
/// same in-memory snapshot + watch channel. Wired into
/// `StorageAdminGrpc` via `with_tuning_store()`.
#[derive(Clone, Debug)]
pub struct TuningStore {
    inner: Arc<RwLock<TuningParams>>,
    persistence: Arc<dyn TuningPersistence>,
    tx: watch::Sender<TuningParams>,
}

impl TuningStore {
    /// Construct with the given persistence backend. Loads the
    /// most-recently-persisted snapshot if any; otherwise starts at
    /// [`TuningParams::default`]. Returns the loaded snapshot
    /// even when it falls outside the current bounds (e.g. after a
    /// schema change tightens a range) — out-of-range loaded values
    /// are clamped to the nearest bound and a warning is logged.
    /// This keeps the server bootable across version skew.
    pub fn with_persistence(persistence: Arc<dyn TuningPersistence>) -> Self {
        let initial = match persistence.load() {
            Ok(Some(p)) => {
                if let Err(e) = p.validate() {
                    tracing::warn!(
                        error = %e,
                        "tuning: loaded params out of current bounds; using defaults"
                    );
                    TuningParams::default()
                } else {
                    p
                }
            }
            Ok(None) => TuningParams::default(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "tuning: persistence load failed; using defaults"
                );
                TuningParams::default()
            }
        };
        let (tx, _rx) = watch::channel(initial);
        Self {
            inner: Arc::new(RwLock::new(initial)),
            persistence,
            tx,
        }
    }

    /// In-memory only — discards on restart. Convenience for tests
    /// and ephemeral single-node deployments.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::with_persistence(Arc::new(InMemoryTuningPersistence))
    }

    /// Snapshot the current params.
    pub async fn get(&self) -> TuningParams {
        *self.inner.read().await
    }

    /// Replace the params. Validates first; on success persists
    /// then swaps; broadcasts via the watch channel for live
    /// subscribers. On validation failure returns the
    /// `Status::invalid_argument` from [`TuningParams::validate`]
    /// without touching backing state — partial updates can't
    /// happen because [`TuningParams`] is replaced atomically.
    pub async fn set(&self, params: TuningParams) -> Result<(), Status> {
        params.validate()?;
        self.persistence
            .persist(&params)
            .map_err(|e| Status::internal(format!("tuning: persist failed: {e}")))?;
        let mut g = self.inner.write().await;
        *g = params;
        // Broadcast best-effort — receivers can lag without
        // blocking the writer (watch::Sender::send only errors when
        // there are no receivers, which is fine for tuning).
        let _ = self.tx.send(params);
        Ok(())
    }

    /// Hand a subscriber a fresh `watch::Receiver`. Subsystems use
    /// this to react to live tuning changes without polling.
    /// Receivers see the *current* value on first `borrow()`; every
    /// subsequent `changed().await` resolves on the next `set()`.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<TuningParams> {
        self.tx.subscribe()
    }
}

impl Default for TuningStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

// ===========================================================================
// Redb-backed persistence — survives server restart. Mirrors the
// PersistentRedbStorage pattern from kiseki-composition: one
// dedicated db file under `<data_dir>/tuning.redb`, single
// `tuning_params` row keyed by a fixed sentinel.
// ===========================================================================

/// Redb-backed tuning persistence. Holds its own database file
/// (`<dir>/tuning.redb`) — disjoint from the composition store
/// because `TuningParams` isn't a composition / view artifact and
/// its lifecycle is entirely admin-driven.
#[derive(Debug)]
pub struct RedbTuningPersistence {
    db: std::sync::Mutex<::redb::Database>,
}

impl RedbTuningPersistence {
    const TABLE: ::redb::TableDefinition<'_, &'static str, &'static [u8]> =
        ::redb::TableDefinition::new("tuning_params");
    const KEY: &'static str = "current";

    /// Open or create `<dir>/tuning.redb`. Creates `dir` if missing.
    pub fn open(dir: &std::path::Path) -> Result<Self, TuningStoreError> {
        std::fs::create_dir_all(dir).map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        let path = dir.join("tuning.redb");
        let db =
            ::redb::Database::create(&path).map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        // Materialize the table so first-time reads don't error.
        let txn = db
            .begin_write()
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        {
            let _ = txn
                .open_table(Self::TABLE)
                .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        Ok(Self {
            db: std::sync::Mutex::new(db),
        })
    }
}

impl TuningPersistence for RedbTuningPersistence {
    fn persist(&self, params: &TuningParams) -> Result<(), TuningStoreError> {
        let bytes =
            postcard::to_allocvec(params).map_err(|e| TuningStoreError::Decode(e.to_string()))?;
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let txn = db
            .begin_write()
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        {
            let mut t = txn
                .open_table(Self::TABLE)
                .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
            t.insert(Self::KEY, bytes.as_slice())
                .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        Ok(())
    }

    fn load(&self) -> Result<Option<TuningParams>, TuningStoreError> {
        use ::redb::ReadableDatabase;
        let db = self
            .db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let txn = db
            .begin_read()
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        let table = txn
            .open_table(Self::TABLE)
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?;
        let Some(g) = table
            .get(Self::KEY)
            .map_err(|e| TuningStoreError::Redb(e.to_string()))?
        else {
            return Ok(None);
        };
        let p: TuningParams =
            postcard::from_bytes(g.value()).map_err(|e| TuningStoreError::Decode(e.to_string()))?;
        Ok(Some(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_in_range() {
        TuningParams::default()
            .validate()
            .expect("ADR-025 defaults must pass bounds checking");
    }

    #[test]
    fn defaults_match_adr_025_table() {
        let d = TuningParams::default();
        assert_eq!(d.compaction_rate_mb_s, 100);
        assert_eq!(d.gc_interval_s, 300);
        assert_eq!(d.rebalance_rate_mb_s, 50);
        assert_eq!(d.scrub_interval_h, 168);
        assert_eq!(d.max_concurrent_repairs, 4);
        assert_eq!(d.stream_proc_poll_ms, 100);
        assert_eq!(d.inline_threshold_bytes, 4096);
        assert_eq!(d.raft_snapshot_interval, 10_000);
    }

    #[test]
    fn proto_round_trip_preserves_all_fields() {
        let original = TuningParams {
            compaction_rate_mb_s: 250,
            gc_interval_s: 600,
            rebalance_rate_mb_s: 100,
            scrub_interval_h: 48,
            max_concurrent_repairs: 8,
            stream_proc_poll_ms: 50,
            inline_threshold_bytes: 8192,
            raft_snapshot_interval: 25_000,
        };
        let round_tripped = TuningParams::from_proto(&original.to_proto());
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn validate_rejects_under_minimum() {
        let p = TuningParams {
            compaction_rate_mb_s: 5, // min 10
            ..TuningParams::default()
        };
        let err = p.validate().expect_err("under min should reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("compaction_rate_mb_s"));
    }

    #[test]
    fn validate_rejects_over_maximum() {
        let p = TuningParams {
            scrub_interval_h: 9999, // max 720
            ..TuningParams::default()
        };
        let err = p.validate().expect_err("over max should reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("scrub_interval_h"));
    }

    #[test]
    fn validate_accepts_exact_boundaries() {
        // Lower bounds.
        let p = TuningParams {
            compaction_rate_mb_s: 10,
            gc_interval_s: 60,
            rebalance_rate_mb_s: 0,
            scrub_interval_h: 24,
            max_concurrent_repairs: 1,
            stream_proc_poll_ms: 10,
            inline_threshold_bytes: 512,
            raft_snapshot_interval: 1000,
        };
        p.validate().expect("lower-bound values must validate");
        // Upper bounds.
        let p = TuningParams {
            compaction_rate_mb_s: 1000,
            gc_interval_s: 3600,
            rebalance_rate_mb_s: 500,
            scrub_interval_h: 720,
            max_concurrent_repairs: 32,
            stream_proc_poll_ms: 1000,
            inline_threshold_bytes: 65_536,
            raft_snapshot_interval: 100_000,
        };
        p.validate().expect("upper-bound values must validate");
    }

    #[test]
    fn validate_rejects_zero_for_non_zero_lower_bound_fields() {
        // Every field except rebalance_rate_mb_s must reject zero.
        let zero = TuningParams {
            compaction_rate_mb_s: 0,
            gc_interval_s: 0,
            rebalance_rate_mb_s: 0,
            scrub_interval_h: 0,
            max_concurrent_repairs: 0,
            stream_proc_poll_ms: 0,
            inline_threshold_bytes: 0,
            raft_snapshot_interval: 0,
        };
        let err = zero.validate().expect_err("zeros mostly out of range");
        assert!(err.message().contains("compaction_rate_mb_s"));
    }

    #[tokio::test]
    async fn store_get_returns_default_when_empty() {
        let s = TuningStore::in_memory();
        assert_eq!(s.get().await, TuningParams::default());
    }

    #[tokio::test]
    async fn store_set_then_get_round_trips() {
        let s = TuningStore::in_memory();
        let p = TuningParams {
            compaction_rate_mb_s: 250,
            ..TuningParams::default()
        };
        s.set(p).await.expect("valid");
        assert_eq!(s.get().await.compaction_rate_mb_s, 250);
    }

    #[tokio::test]
    async fn store_set_rejects_out_of_range_without_persisting() {
        let s = TuningStore::in_memory();
        let p = TuningParams {
            scrub_interval_h: 9999,
            ..TuningParams::default()
        };
        s.set(p).await.expect_err("must reject");
        // Defaults should still be intact.
        assert_eq!(s.get().await, TuningParams::default());
    }

    #[tokio::test]
    async fn store_subscribe_receives_change_notifications() {
        let s = TuningStore::in_memory();
        let mut rx = s.subscribe();
        let p = TuningParams {
            gc_interval_s: 600,
            ..TuningParams::default()
        };
        s.set(p).await.expect("valid");
        rx.changed().await.expect("change notification");
        assert_eq!(rx.borrow().gc_interval_s, 600);
    }

    #[test]
    fn redb_persistence_round_trips_across_open() {
        let dir = tempfile::tempdir().expect("tmp");
        // Write side.
        {
            let p = RedbTuningPersistence::open(dir.path()).expect("open");
            let params = TuningParams {
                compaction_rate_mb_s: 333,
                scrub_interval_h: 96,
                ..TuningParams::default()
            };
            p.persist(&params).expect("persist");
            // Drop p → release the file lock.
        }
        // Read side — re-open, must observe.
        let p = RedbTuningPersistence::open(dir.path()).expect("re-open");
        let loaded = p.load().expect("load").expect("Some");
        assert_eq!(loaded.compaction_rate_mb_s, 333);
        assert_eq!(loaded.scrub_interval_h, 96);
    }

    #[test]
    fn redb_persistence_load_empty_returns_none() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = RedbTuningPersistence::open(dir.path()).expect("open");
        assert!(p.load().expect("load").is_none());
    }

    #[tokio::test]
    async fn store_with_redb_persistence_rehydrates_on_restart() {
        let dir = tempfile::tempdir().expect("tmp");
        // Boot 1: customise + set.
        {
            let p = Arc::new(RedbTuningPersistence::open(dir.path()).expect("open"));
            let s = TuningStore::with_persistence(p);
            let params = TuningParams {
                gc_interval_s: 1200,
                ..TuningParams::default()
            };
            s.set(params).await.expect("valid");
        }
        // Boot 2: re-open with same dir.
        let p = Arc::new(RedbTuningPersistence::open(dir.path()).expect("re-open"));
        let s = TuningStore::with_persistence(p);
        assert_eq!(s.get().await.gc_interval_s, 1200);
    }

    #[tokio::test]
    async fn store_with_corrupted_persistence_falls_back_to_defaults() {
        // A persistence impl that returns an out-of-range value.
        #[derive(Debug)]
        struct BadPersistence;
        impl TuningPersistence for BadPersistence {
            fn persist(&self, _: &TuningParams) -> Result<(), TuningStoreError> {
                Ok(())
            }
            fn load(&self) -> Result<Option<TuningParams>, TuningStoreError> {
                let p = TuningParams {
                    scrub_interval_h: 9999, // out of range
                    ..TuningParams::default()
                };
                Ok(Some(p))
            }
        }
        let s = TuningStore::with_persistence(Arc::new(BadPersistence));
        // Should clamp to defaults so the server boots.
        assert_eq!(s.get().await, TuningParams::default());
    }
}
