//! Client-side topology cache (ADR-042 §5, A-NG13, I-NG13).
//!
//! Holds a snapshot of `(version, nodes, shards)` from the most recent
//! `GetTopology` call. Every native RPC response carries
//! `kiseki-topology-version` in the gRPC trailing metadata; the client
//! peeks at it and refreshes asynchronously when it diverges. A 30 s
//! TTL safety net guarantees the cache eventually re-reads even if no
//! response comes in (idle clients).
//!
//! Phase 5 ships the in-memory cache + version-bump bookkeeping. The
//! refresh task wiring (a tokio task that re-fetches on version-diff)
//! is left to Phase 5+ once the `NativeClient` channel is in place.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;

/// One node, as the server reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    /// Cluster-internal node id.
    pub node_id: u64,
    /// `host:port` for the data-path port.
    pub data_addr: String,
}

/// One shard's leadership tuple.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shard {
    /// Shard identifier (UUID-string form, matching proto encoding).
    pub shard_id: String,
    /// Current leader's node id.
    pub leader_node_id: u64,
    /// Inclusive lower bound of the shard's hashed-key range.
    pub range_start: Vec<u8>,
    /// Exclusive upper bound.
    pub range_end: Vec<u8>,
}

/// Cached snapshot. `version == 0` means "never populated"; the very
/// first `GetTopology` produces version >= 1.
#[allow(missing_docs)]
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub version: u64,
    pub nodes: Vec<Node>,
    pub shards: Vec<Shard>,
}

/// Process-wide topology cache. Designed so reads (`current_version`,
/// `route_for_hashed_key`) hit no contention on the hot path —
/// `parking_lot::RwLock<Snapshot>` lets readers proceed in parallel.
#[derive(Debug)]
pub struct TopologyCache {
    snapshot: RwLock<Snapshot>,
    /// Independent atomic for the hot-path version compare. Keeping
    /// the version out of the `RwLock` means the trailer-peek path
    /// (per-RPC) doesn't take any lock.
    version: AtomicU64,
    /// Last time the cache refreshed. Used by the 30 s TTL.
    last_refresh: RwLock<Instant>,
    /// TTL between forced refreshes — clients tweak via
    /// `with_ttl(...)`.
    ttl: Duration,
}

impl Default for TopologyCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TopologyCache {
    /// Empty cache; first `GetTopology` will fully populate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(Snapshot::default()),
            version: AtomicU64::new(0),
            last_refresh: RwLock::new(Instant::now()),
            ttl: Duration::from_secs(30),
        }
    }

    /// Override the safety-net TTL.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Atomic version snapshot — taken on every RPC trailer compare.
    #[must_use]
    pub fn current_version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Replace the cache with a fresh snapshot. Atomically bumps
    /// `version` and resets the TTL clock.
    pub fn replace(&self, snap: Snapshot) {
        // Update the snapshot first so readers that observe the new
        // `version` always see the matching `nodes`/`shards`.
        let new_version = snap.version;
        *self.snapshot.write() = snap;
        self.version.store(new_version, Ordering::Release);
        *self.last_refresh.write() = Instant::now();
    }

    /// Snapshot the current cache. Cheap (`Snapshot: Clone`).
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().clone()
    }

    /// Whether the TTL has expired since the last refresh.
    #[must_use]
    pub fn ttl_expired(&self) -> bool {
        Instant::now().duration_since(*self.last_refresh.read()) > self.ttl
    }

    /// Decide whether the cache needs a refresh, given the version
    /// stamped on the most recent RPC trailer. Returns:
    /// - `RefreshDecision::FreshEnough` — versions match AND TTL valid.
    /// - `RefreshDecision::TrailerVersionDiffers` — kick a refresh.
    /// - `RefreshDecision::TtlExpired` — kick a refresh anyway.
    #[must_use]
    pub fn decide(&self, trailer_version: u64) -> RefreshDecision {
        let cached = self.current_version();
        if trailer_version != 0 && trailer_version != cached {
            return RefreshDecision::TrailerVersionDiffers {
                cached,
                seen: trailer_version,
            };
        }
        if self.ttl_expired() {
            return RefreshDecision::TtlExpired;
        }
        RefreshDecision::FreshEnough
    }

    /// Find the node currently leading the shard whose key range
    /// contains `hashed_key`. Returns `None` if the cache is empty
    /// or the key falls outside every cached shard range (the
    /// caller should kick a refresh and retry).
    #[must_use]
    pub fn route_for_hashed_key(&self, hashed_key: &[u8]) -> Option<RouteHit> {
        let snap = self.snapshot.read();
        let shard = snap
            .shards
            .iter()
            .find(|s| key_in_range(hashed_key, &s.range_start, &s.range_end))?;
        let node = snap.nodes.iter().find(|n| n.node_id == shard.leader_node_id)?;
        Some(RouteHit {
            shard_id: shard.shard_id.clone(),
            leader_node_id: node.node_id,
            data_addr: node.data_addr.clone(),
        })
    }
}

/// Outcome of [`TopologyCache::decide`].
#[allow(missing_docs)]
#[derive(Debug, Eq, PartialEq)]
pub enum RefreshDecision {
    FreshEnough,
    TrailerVersionDiffers { cached: u64, seen: u64 },
    TtlExpired,
}

/// Successful routing. The native client dials `data_addr` and
/// includes `shard_id` in audit / metrics.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteHit {
    pub shard_id: String,
    pub leader_node_id: u64,
    pub data_addr: String,
}

fn key_in_range(key: &[u8], start: &[u8], end: &[u8]) -> bool {
    // [start, end) over byte-string ordering. An all-zeros end is a
    // sentinel for "no upper bound".
    let above_start = key >= start;
    let below_end = end.is_empty() || key < end;
    above_start && below_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(version: u64) -> Snapshot {
        Snapshot {
            version,
            nodes: vec![Node {
                node_id: 1,
                data_addr: "127.0.0.1:9100".into(),
            }],
            shards: vec![Shard {
                shard_id: "shard-1".into(),
                leader_node_id: 1,
                range_start: vec![],
                range_end: vec![],
            }],
        }
    }

    #[test]
    fn replace_bumps_version_and_returns_via_snapshot() {
        let cache = TopologyCache::new();
        assert_eq!(cache.current_version(), 0);
        cache.replace(snap(7));
        assert_eq!(cache.current_version(), 7);
        let s = cache.snapshot();
        assert_eq!(s.version, 7);
        assert_eq!(s.nodes.len(), 1);
    }

    #[test]
    fn decide_fresh_enough_when_versions_match() {
        let cache = TopologyCache::new().with_ttl(Duration::from_secs(60));
        cache.replace(snap(3));
        assert_eq!(cache.decide(3), RefreshDecision::FreshEnough);
    }

    #[test]
    fn decide_kicks_refresh_on_version_diff() {
        let cache = TopologyCache::new().with_ttl(Duration::from_secs(60));
        cache.replace(snap(3));
        let d = cache.decide(7);
        assert!(matches!(
            d,
            RefreshDecision::TrailerVersionDiffers {
                cached: 3,
                seen: 7
            }
        ));
    }

    #[test]
    fn decide_kicks_refresh_on_ttl_expired() {
        let cache = TopologyCache::new().with_ttl(Duration::from_millis(1));
        cache.replace(snap(3));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.decide(3), RefreshDecision::TtlExpired);
    }

    #[test]
    fn route_for_key_returns_leader_for_full_range_shard() {
        let cache = TopologyCache::new();
        cache.replace(snap(1));
        let hit = cache.route_for_hashed_key(&[0xff; 8]).unwrap();
        assert_eq!(hit.leader_node_id, 1);
        assert_eq!(hit.data_addr, "127.0.0.1:9100");
    }

    #[test]
    fn route_for_key_returns_none_when_key_outside_range() {
        let cache = TopologyCache::new();
        let mut s = snap(1);
        s.shards[0].range_start = vec![0xa0];
        s.shards[0].range_end = vec![0xb0];
        cache.replace(s);
        assert!(cache.route_for_hashed_key(&[0xc0]).is_none());
        assert!(cache.route_for_hashed_key(&[0xa5]).is_some());
    }
}
