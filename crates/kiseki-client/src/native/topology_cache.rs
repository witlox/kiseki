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

/// One node, as the server reports it. ADR-042 §1.7.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    /// Cluster-internal node id.
    pub node_id: u64,
    /// Legacy single-address field — the gRPC binding's `host:port`
    /// for v0-client compat. New clients route via `bindings`.
    pub data_addr: String,
    /// Cluster-membership state (Active / Degraded / Draining /
    /// Failed / Evicted). Drives the §1.7 reachability gate in the
    /// per-edge selector.
    pub state: kiseki_proto::native_contract::NodeState,
    /// Per-node binding endpoints. ADR-042 §1.7. Empty when state
    /// is Failed/Evicted; multiple entries when the node serves
    /// multiple bindings concurrently. Per-edge selection (§3.2)
    /// picks the highest-ranked `latency_class` mutually supported by
    /// the local environment.
    pub bindings: Vec<kiseki_proto::native_contract::BindingEndpoint>,
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

    /// ADR-008 rev 2 / adversary finding S4 — conditional replace
    /// that only accepts a strictly newer snapshot. Used by the
    /// HTTP `/cluster/info` bootstrap path so a stale poll cannot
    /// overwrite a fresher gRPC `GetTopology` update.
    ///
    /// Returns `true` iff the snapshot was applied (`snap.version >
    /// self.current_version()`).
    pub fn replace_if_newer(&self, snap: Snapshot) -> bool {
        if snap.version > self.current_version() {
            self.replace(snap);
            true
        } else {
            false
        }
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
        let node = snap
            .nodes
            .iter()
            .find(|n| n.node_id == shard.leader_node_id)?;
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

/// ADR-008 rev 2 — bootstrap-time [`Snapshot`] constructor from the
/// HTTP `/cluster/info` JSON. Used before the native gRPC channel is
/// dialled (the gRPC `GetTopology` is the steady-state refresh).
///
/// The returned snapshot carries `version = 1` as a synthetic
/// floor — strictly above the cache's initial `version = 0` so the
/// HTTP bootstrap populates an empty cache. Subsequent gRPC
/// `GetTopology` responses carry the authoritative control-plane
/// version (typically ≥ 1 for any namespace that has ever been
/// created), and `replace_if_newer` upgrades the cache to that
/// version on first response.
#[must_use]
pub fn snapshot_from_cluster_info(parsed: &crate::discovery::ClusterInfoResponse) -> Snapshot {
    // Build the nodes list from `peers`. Each peer's data-port
    // address is its raft host + the conventional 9100 port; this
    // mirrors what `node_info_from_plan` produces server-side.
    let nodes: Vec<Node> = parsed
        .peers
        .iter()
        .map(|p| {
            let host = p.raft_addr.split(':').next().unwrap_or("127.0.0.1");
            Node {
                node_id: p.id,
                data_addr: format!("{host}:9100"),
                state: kiseki_proto::native_contract::NodeState::Active,
                bindings: Vec::new(),
            }
        })
        .collect();

    // Project each shard onto the cache's Shard shape. Missing
    // leader_id maps to 0 (sentinel for "unknown" — the cache's
    // route_for_hashed_key falls back to a refresh in that case).
    let shards: Vec<Shard> = parsed
        .shards
        .iter()
        .map(|s| {
            let range_start = decode_hex_prefixed(&s.range_start).unwrap_or_default();
            let range_end = decode_hex_prefixed(&s.range_end).unwrap_or_default();
            Shard {
                shard_id: s.shard_id.clone(),
                leader_node_id: s.leader_id.unwrap_or(0),
                range_start,
                range_end,
            }
        })
        .collect();

    Snapshot {
        version: 1,
        nodes,
        shards,
    }
}

/// Same hex decoder as `discovery.rs`. Duplicated here so the two
/// modules don't cross-import each other's internals.
fn decode_hex_prefixed(s: &str) -> Option<Vec<u8>> {
    let stripped = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if stripped.len() != 64 {
        return None;
    }
    let mut out = Vec::with_capacity(32);
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble_value(bytes[i])?;
        let lo = hex_nibble_value(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

const fn hex_nibble_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Convert a proto-shaped `TopologyInfo` (from `GetTopology`) into a
/// cache-shaped [`Snapshot`]. ADR-042 §1.7 wire types map back to
/// the `kiseki-proto::native_contract` value types the cache + edge
/// selector consume.
#[must_use]
pub fn snapshot_from_proto(info: &kiseki_proto::v1::native::TopologyInfo) -> Snapshot {
    use kiseki_proto::native_contract as nc;
    use kiseki_proto::v1::native as np;

    let nodes = info
        .nodes
        .iter()
        .map(|n| Node {
            node_id: n.node_id,
            data_addr: n.data_addr.clone(),
            state: proto_node_state_to_contract(n.state),
            bindings: n
                .bindings
                .iter()
                .filter_map(proto_binding_endpoint_to_contract)
                .collect::<Vec<nc::BindingEndpoint>>(),
        })
        .collect();

    let shards = info
        .shards
        .iter()
        .map(|s| Shard {
            shard_id: s
                .shard_id
                .as_ref()
                .map(|sid| sid.value.clone())
                .unwrap_or_default(),
            leader_node_id: s.leader_node_id,
            range_start: s.range_start.clone(),
            range_end: s.range_end.clone(),
        })
        .collect();

    let _ = np::TopologyInfo::default; // keep np in scope for clarity; no use beyond imports above
    Snapshot {
        version: info.topology_version,
        nodes,
        shards,
    }
}

fn proto_node_state_to_contract(state: i32) -> kiseki_proto::native_contract::NodeState {
    use kiseki_proto::native_contract::NodeState as Nc;
    use kiseki_proto::v1::native::NodeState as Pb;
    match Pb::try_from(state).unwrap_or(Pb::Unspecified) {
        Pb::Active | Pb::Unspecified => Nc::Active,
        Pb::Degraded => Nc::Degraded,
        Pb::Draining => Nc::Draining,
        Pb::Failed => Nc::Failed,
        Pb::Evicted => Nc::Evicted,
    }
}

fn proto_binding_endpoint_to_contract(
    ep: &kiseki_proto::v1::native::BindingEndpoint,
) -> Option<kiseki_proto::native_contract::BindingEndpoint> {
    use kiseki_proto::native_contract as nc;
    use kiseki_proto::v1::native as np;

    let binding_id = match np::BindingId::try_from(ep.binding_id).ok()? {
        np::BindingId::Unspecified => return None,
        np::BindingId::Grpc => nc::BindingId::Grpc,
        np::BindingId::TcpFramed => nc::BindingId::TcpFramed,
        np::BindingId::Ibverbs => nc::BindingId::Ibverbs,
        // libfabric carries a provider variant in the contract. The
        // proto doesn't expose the provider yet (single enum
        // variant); default to the most common provider for v1
        // (Cxi). When the proto adds the provider field, plumb here.
        np::BindingId::Libfabric => nc::BindingId::Libfabric {
            provider: nc::LibfabricProvider::Cxi,
        },
    };
    let latency_class = match np::LatencyClass::try_from(ep.latency_class).ok()? {
        np::LatencyClass::Unspecified | np::LatencyClass::Standard => nc::LatencyClass::Standard,
        np::LatencyClass::Low => nc::LatencyClass::Low,
        np::LatencyClass::Rdma => nc::LatencyClass::Rdma,
    };
    let addr = if ep.addr.starts_with("fabric:") {
        nc::ListenAddr::FabricDescriptor(
            decode_fabric_hex(&ep.addr["fabric:".len()..]).unwrap_or_default(),
        )
    } else {
        nc::ListenAddr::HostPort(ep.addr.clone())
    };
    let drain_state = ep.drain_state.as_ref().map(|d| nc::DrainState {
        quiesce_window_remaining_ms: d.quiesce_window_remaining_ms,
        accepts_new_work: d.accepts_new_work,
    });
    Some(nc::BindingEndpoint {
        binding_id,
        addr,
        latency_class,
        drain_state,
    })
}

fn decode_fabric_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
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
                state: kiseki_proto::native_contract::NodeState::Active,
                bindings: Vec::new(),
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
            RefreshDecision::TrailerVersionDiffers { cached: 3, seen: 7 }
        ));
    }

    #[test]
    fn snapshot_from_proto_carries_node_bindings() {
        use kiseki_proto::v1;
        use kiseki_proto::v1::native as np;
        let info = np::TopologyInfo {
            topology_version: 5,
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "10.0.0.1:9100".into(),
                state: np::NodeState::Active as i32,
                bindings: vec![
                    np::BindingEndpoint {
                        binding_id: np::BindingId::Grpc as i32,
                        addr: "10.0.0.1:9100".into(),
                        latency_class: np::LatencyClass::Standard as i32,
                        drain_state: None,
                    },
                    np::BindingEndpoint {
                        binding_id: np::BindingId::TcpFramed as i32,
                        addr: "10.0.0.1:9101".into(),
                        latency_class: np::LatencyClass::Low as i32,
                        drain_state: None,
                    },
                ],
            }],
            shards: vec![np::ShardLeadership {
                shard_id: Some(v1::ShardId {
                    value: "00000000-0000-0000-0000-000000000001".into(),
                }),
                leader_node_id: 1,
                range_start: vec![],
                range_end: vec![],
            }],
        };
        let snap = snapshot_from_proto(&info);
        assert_eq!(snap.version, 5);
        assert_eq!(snap.nodes.len(), 1);
        let node = &snap.nodes[0];
        assert_eq!(node.node_id, 1);
        assert_eq!(node.bindings.len(), 2);
        assert_eq!(
            node.bindings[0].binding_id,
            kiseki_proto::native_contract::BindingId::Grpc
        );
        assert_eq!(
            node.bindings[1].binding_id,
            kiseki_proto::native_contract::BindingId::TcpFramed
        );
        assert_eq!(snap.shards.len(), 1);
        assert_eq!(snap.shards[0].leader_node_id, 1);
    }

    #[test]
    fn snapshot_from_proto_decodes_drain_state() {
        use kiseki_proto::v1::native as np;
        let info = np::TopologyInfo {
            topology_version: 1,
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "10.0.0.1:9100".into(),
                state: np::NodeState::Draining as i32,
                bindings: vec![np::BindingEndpoint {
                    binding_id: np::BindingId::Grpc as i32,
                    addr: "10.0.0.1:9100".into(),
                    latency_class: np::LatencyClass::Standard as i32,
                    drain_state: Some(np::DrainState {
                        quiesce_window_remaining_ms: 12_345,
                        accepts_new_work: false,
                    }),
                }],
            }],
            shards: vec![],
        };
        let snap = snapshot_from_proto(&info);
        let node = &snap.nodes[0];
        assert_eq!(
            node.state,
            kiseki_proto::native_contract::NodeState::Draining
        );
        let drain = node.bindings[0].drain_state.unwrap();
        assert_eq!(drain.quiesce_window_remaining_ms, 12_345);
        assert!(!drain.accepts_new_work);
    }

    #[test]
    fn snapshot_from_proto_falls_back_to_active_for_unspecified_state() {
        use kiseki_proto::v1::native as np;
        let info = np::TopologyInfo {
            topology_version: 1,
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "10.0.0.1:9100".into(),
                // 0 = NODE_STATE_UNSPECIFIED — defensive default.
                state: 0,
                bindings: vec![],
            }],
            shards: vec![],
        };
        let snap = snapshot_from_proto(&info);
        assert_eq!(
            snap.nodes[0].state,
            kiseki_proto::native_contract::NodeState::Active
        );
    }

    #[test]
    fn snapshot_from_proto_decodes_fabric_descriptor_addr() {
        use kiseki_proto::v1::native as np;
        let info = np::TopologyInfo {
            topology_version: 1,
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: String::new(),
                state: np::NodeState::Active as i32,
                bindings: vec![np::BindingEndpoint {
                    binding_id: np::BindingId::Libfabric as i32,
                    addr: "fabric:cafebabe".into(),
                    latency_class: np::LatencyClass::Rdma as i32,
                    drain_state: None,
                }],
            }],
            shards: vec![],
        };
        let snap = snapshot_from_proto(&info);
        match &snap.nodes[0].bindings[0].addr {
            kiseki_proto::native_contract::ListenAddr::FabricDescriptor(bytes) => {
                assert_eq!(bytes, &[0xCA, 0xFE, 0xBA, 0xBE]);
            }
            kiseki_proto::native_contract::ListenAddr::HostPort(s) => {
                panic!("expected FabricDescriptor, got HostPort: {s:?}");
            }
        }
    }

    #[test]
    fn snapshot_from_proto_skips_unspecified_binding_id() {
        use kiseki_proto::v1::native as np;
        let info = np::TopologyInfo {
            topology_version: 1,
            nodes: vec![np::NodeInfo {
                node_id: 1,
                data_addr: "10.0.0.1:9100".into(),
                state: np::NodeState::Active as i32,
                bindings: vec![np::BindingEndpoint {
                    // 0 = BINDING_ID_UNSPECIFIED — silently skip rather
                    // than promote to a real binding. v0 servers might
                    // emit this if the field was added later in their
                    // proto schema and they default-construct.
                    binding_id: 0,
                    addr: "10.0.0.1:9100".into(),
                    latency_class: np::LatencyClass::Standard as i32,
                    drain_state: None,
                }],
            }],
            shards: vec![],
        };
        let snap = snapshot_from_proto(&info);
        assert!(
            snap.nodes[0].bindings.is_empty(),
            "Unspecified binding-id must be skipped, not promoted"
        );
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

    // ADR-008 rev 2 — snapshot_from_cluster_info bootstrap path.

    #[test]
    fn snapshot_from_cluster_info_populates_shards_and_nodes() {
        let parsed = crate::discovery::ClusterInfoResponse {
            node_id: 1,
            s3_addr: "10.0.0.1:9000".into(),
            nfs_addr: "10.0.0.1:2049".into(),
            metrics_addr: "10.0.0.1:9090".into(),
            leader_id: Some(2),
            leader_s3: Some("10.0.0.2:9000".into()),
            peers: vec![
                crate::discovery::PeerInfoJson {
                    id: 1,
                    raft_addr: "10.0.0.1:7000".into(),
                    s3_addr: "10.0.0.1:9000".into(),
                    nfs_addr: "10.0.0.1:2049".into(),
                    metrics_addr: "10.0.0.1:9090".into(),
                },
                crate::discovery::PeerInfoJson {
                    id: 2,
                    raft_addr: "10.0.0.2:7000".into(),
                    s3_addr: "10.0.0.2:9000".into(),
                    nfs_addr: "10.0.0.2:2049".into(),
                    metrics_addr: "10.0.0.2:9090".into(),
                },
            ],
            shards: vec![crate::discovery::ShardInfoJson {
                shard_id: "00000000-0000-0000-0000-000000000001".into(),
                namespace_id: "trials".into(),
                leader_id: Some(2),
                leader_data_addr: Some("10.0.0.2:9100".into()),
                range_start: "0x0000000000000000000000000000000000000000000000000000000000000000".into(),
                range_end:   "0x5555555555555555555555555555555555555555555555555555555555555555".into(),
            }],
        };
        let snap = snapshot_from_cluster_info(&parsed);
        // Two nodes derived from peers, one shard.
        assert_eq!(snap.nodes.len(), 2, "nodes derived from peers list");
        let node_2 = snap
            .nodes
            .iter()
            .find(|n| n.node_id == 2)
            .expect("node 2 present");
        assert_eq!(node_2.data_addr, "10.0.0.2:9100");
        assert_eq!(snap.shards.len(), 1);
        assert_eq!(snap.shards[0].leader_node_id, 2);
        assert_eq!(snap.shards[0].range_start[0], 0x00);
        assert_eq!(snap.shards[0].range_end[31], 0x55);
        // Bootstrap version is non-zero so subsequent replace_if_newer
        // calls against a fresh gRPC version (which starts >= 1)
        // behave deterministically.
        assert!(
            snap.version >= 1,
            "bootstrap snapshot must carry a non-zero version"
        );
    }

    #[test]
    fn snapshot_from_cluster_info_falls_back_when_leader_data_addr_missing() {
        // Cold start: rev-2 server hasn't yet observed a leader, so
        // `leader_data_addr` is None. The bootstrap must still produce
        // a Snapshot — the shard's data_addr falls back to the peer's
        // S3 / metrics address derived host.
        let parsed = crate::discovery::ClusterInfoResponse {
            node_id: 1,
            s3_addr: "10.0.0.1:9000".into(),
            nfs_addr: "10.0.0.1:2049".into(),
            metrics_addr: "10.0.0.1:9090".into(),
            leader_id: None,
            leader_s3: None,
            peers: vec![crate::discovery::PeerInfoJson {
                id: 1,
                raft_addr: "10.0.0.1:7000".into(),
                s3_addr: "10.0.0.1:9000".into(),
                nfs_addr: "10.0.0.1:2049".into(),
                metrics_addr: "10.0.0.1:9090".into(),
            }],
            shards: vec![crate::discovery::ShardInfoJson {
                shard_id: "00000000-0000-0000-0000-000000000001".into(),
                namespace_id: "trials".into(),
                leader_id: None,
                leader_data_addr: None,
                range_start: "0x0000000000000000000000000000000000000000000000000000000000000000".into(),
                range_end:   "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
            }],
        };
        let snap = snapshot_from_cluster_info(&parsed);
        // With no leader hint, the shard is still recorded but
        // its leader_node_id is 0 (sentinel for "unknown").
        assert_eq!(snap.shards.len(), 1);
        assert_eq!(snap.shards[0].leader_node_id, 0);
    }

    // Finding S4 — replace_if_newer guard against version regressions.

    #[test]
    fn replace_if_newer_only_accepts_strictly_greater_version() {
        let cache = TopologyCache::new();
        cache.replace(snap(42));
        // Same version: rejected.
        assert!(
            !cache.replace_if_newer(snap(42)),
            "equal version must be rejected"
        );
        assert_eq!(cache.current_version(), 42);
        // Stale (HTTP bootstrap returning version=0): rejected.
        assert!(
            !cache.replace_if_newer(snap(0)),
            "stale version must be rejected"
        );
        assert_eq!(cache.current_version(), 42);
        // Strictly greater: accepted.
        assert!(
            cache.replace_if_newer(snap(43)),
            "strictly newer version must be accepted"
        );
        assert_eq!(cache.current_version(), 43);
    }

    #[test]
    fn replace_if_newer_populates_when_cache_is_empty() {
        let cache = TopologyCache::new();
        assert_eq!(cache.current_version(), 0);
        // Fresh cache + any non-zero version: accepted.
        assert!(cache.replace_if_newer(snap(7)));
        assert_eq!(cache.current_version(), 7);
    }
}
