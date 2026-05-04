//! Per-shard maintenance-mode flag (ADR-025 W4 — `SetShardMaintenance`).
//!
//! When a shard is in maintenance mode, write RPCs (`PutFragment`,
//! `WriteChunk`, `WriteChunkEc`) return `FailedPrecondition` so
//! operators can drain in-flight work, replace a faulty disk, or
//! quiesce the data path before reconfiguration. Read RPCs are
//! unaffected — clients can still serve hot data while maintenance
//! is in progress.
//!
//! ## State model
//!
//! [`MaintenanceMode`] is an `Arc`-shared `DashMap<ShardId,
//! AtomicBool>`. Today the cluster runs a single bootstrap shard,
//! so the map has at most one entry; ADR-033 / ADR-034 will add
//! multi-shard semantics where each entry corresponds to one
//! Raft group.
//!
//! ## Cluster scope (W4 vs W5)
//!
//! W4 lands the flag as a *node-local* mutation — the storage
//! admin RPC flips it on the receiving node only. Writes routed
//! through this node's `ClusterChunkServer` will reject; writes
//! routed through a peer will not see the flag.
//!
//! W5 elevates the flag to a Raft-coordinated delta on the
//! cluster control shard so every node converges. The
//! [`MaintenanceMode`] type stays the same — the W5 hydrator just
//! calls `set()` from its apply step.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use kiseki_common::ids::ShardId;
use kiseki_common::locks::LockOrDie;

/// Per-shard maintenance-mode flag store. Cheap to clone via
/// `Arc`. See module docs for cluster-scope caveats.
#[derive(Debug, Default)]
pub struct MaintenanceMode {
    inner: Mutex<HashMap<ShardId, AtomicBool>>,
}

impl MaintenanceMode {
    /// Construct an empty store. No shards are in maintenance until
    /// `set()` is called.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Flip the maintenance flag for `shard`. Idempotent. Creates
    /// the per-shard entry on first call. Returns the previous
    /// value (default `false`).
    pub fn set(&self, shard: ShardId, enabled: bool) -> bool {
        let mut g = self.inner.lock().lock_or_die("maintenance.inner");
        let entry = g.entry(shard).or_insert_with(|| AtomicBool::new(false));
        // SeqCst across reads/writes — operators flipping
        // maintenance want immediate global visibility on the
        // node, even at the cost of a couple of cache lines.
        entry.swap(enabled, Ordering::SeqCst)
    }

    /// Snapshot the current flag for `shard`. Returns `false` if
    /// the shard has never been touched (default).
    #[must_use]
    pub fn is_in_maintenance(&self, shard: ShardId) -> bool {
        let g = self.inner.lock().lock_or_die("maintenance.inner");
        g.get(&shard).is_some_and(|f| f.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard(b: u8) -> ShardId {
        ShardId(uuid::Uuid::from_u128(u128::from(b)))
    }

    #[test]
    fn fresh_store_reports_no_maintenance() {
        let m = MaintenanceMode::new();
        assert!(!m.is_in_maintenance(shard(1)));
    }

    #[test]
    fn set_true_then_query_returns_true() {
        let m = MaintenanceMode::new();
        let prev = m.set(shard(1), true);
        assert!(!prev, "first set returns previous default false");
        assert!(m.is_in_maintenance(shard(1)));
    }

    #[test]
    fn set_false_clears_flag() {
        let m = MaintenanceMode::new();
        m.set(shard(1), true);
        let prev = m.set(shard(1), false);
        assert!(prev, "set false returns previous true");
        assert!(!m.is_in_maintenance(shard(1)));
    }

    #[test]
    fn flag_is_per_shard() {
        let m = MaintenanceMode::new();
        m.set(shard(1), true);
        assert!(m.is_in_maintenance(shard(1)));
        assert!(
            !m.is_in_maintenance(shard(2)),
            "shard 2 must not see shard 1's maintenance flag",
        );
    }

    #[test]
    fn set_is_idempotent() {
        let m = MaintenanceMode::new();
        m.set(shard(1), true);
        let prev = m.set(shard(1), true);
        assert!(prev, "second set true returns previous true");
        assert!(m.is_in_maintenance(shard(1)));
    }
}
