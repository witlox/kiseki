//! Per-edge connection pool (ADR-042 §3.2 + §3.2.2).
//!
//! Keyed by `(node_id, BindingId)`. The pool consults the edge
//! selector for each `(client → node)` edge: the SAME node can have
//! up to four entries in the pool concurrently (gRPC + TCP-framed +
//! ibverbs + libfabric/cxi), one per binding the node advertises and
//! the local environment supports. The selector picks per-edge per
//! §3.2; the pool caches the dialed connection so the next call to
//! the same edge skips re-dialing.
//!
//! Connection lifecycle:
//! - **Open**: [`ConnectionPool::get_or_dial`] looks up
//!   `(node_id, binding_id)`; on miss, dials per the binding type
//!   (tonic `Channel::from_shared` for gRPC; `TcpFramedClient::
//!   connect_*` for TCP-framed) and caches.
//! - **Close**: [`ConnectionPool::drop_edge`] removes the entry and
//!   drops the connection per the §3.2.2 per-binding hard-close
//!   discipline. For TCP-based bindings (gRPC, TCP-framed), the
//!   kernel handles cleanup; future RDMA bindings will need the
//!   ordered close steps in §3.2.2's table.
//!
//! Drain protocol per §3.2.1 is the next slice — the pool exposes
//! the entry-set so a future `drain_binding_set_change` can iterate
//! over `(node_id, binding_id)` pairs the topology no longer
//! advertises and transition them to drain mode.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use kiseki_proto::native_contract::{BindingEndpoint, BindingId, ListenAddr};

use super::tcp_framed::TcpFramedClient;
use super::topology_cache::Snapshot;

/// Default drain budget per ADR-042 §3.2.1
/// (`KISEKI_NATIVE_DRAIN_BUDGET_MS`). Edges past this since
/// transitioning to drain mode are hard-closed at the next
/// [`ConnectionPool::tick_drain_budget`] call.
pub const DEFAULT_DRAIN_BUDGET_MS: u64 = 30_000;

/// One pooled connection. The variant tag IS the binding id —
/// callers dispatch on it to issue verbs against the right
/// binding's wire protocol.
#[derive(Clone)]
pub enum Connection {
    /// gRPC/h2 over rustls/TCP (ADR-042 §2.1). The tonic channel
    /// is shared via clone — internal to tonic the channel is an
    /// Arc-backed pool of connections, so cloning is cheap.
    Grpc(tonic::transport::Channel),
    /// TCP-framed-postcard over rustls/TCP (ADR-042 §2.2). The
    /// client owns one persistent TCP connection with multiplexed
    /// `request_id`-correlated requests.
    TcpFramed(Arc<TcpFramedClient>),
    // Future variants when the corresponding binding lands:
    // Ibverbs(Arc<IbverbsClient>),
    // Libfabric(Arc<LibfabricClient>),
}

impl Connection {
    /// Stable binding identifier for audit + metrics attribution.
    #[must_use]
    pub fn binding_id(&self) -> BindingId {
        match self {
            Self::Grpc(_) => BindingId::Grpc,
            Self::TcpFramed(_) => BindingId::TcpFramed,
        }
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Grpc(_) => f.debug_tuple("Grpc").field(&"<tonic::Channel>").finish(),
            Self::TcpFramed(_) => f
                .debug_tuple("TcpFramed")
                .field(&"<Arc<TcpFramedClient>>")
                .finish(),
        }
    }
}

/// Errors raised by the connection pool. Wraps the binding-specific
/// dial errors into a single error type so callers don't need to
/// match per-binding.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// gRPC dial failed (TLS handshake, DNS, connect timeout, etc.).
    #[error("grpc dial failed: {0}")]
    Grpc(String),
    /// TCP-framed dial failed.
    #[error("tcp-framed dial failed: {0}")]
    TcpFramed(String),
    /// The edge's address is a fabric descriptor (RDMA), not a
    /// host:port — the IP-binding dial paths can't consume it.
    /// Callers should self-disqualify the edge or wait for RDMA
    /// support (phase 9/10).
    #[error("fabric-descriptor address not supported by IP bindings: {0:?}")]
    UnsupportedFabricAddr(Vec<u8>),
    /// Unsupported binding (ibverbs / libfabric) — the pool was
    /// asked to dial a binding the v1 build doesn't know how to
    /// dial yet. Distinct from `UnsupportedFabricAddr`: the binding
    /// itself isn't compiled in.
    #[error("binding {binding_id:?} not yet supported by client pool")]
    UnsupportedBinding {
        /// Which binding the caller asked for.
        binding_id: BindingId,
    },
}

/// Per-edge connection pool. Cheap to clone via `Arc<ConnectionPool>`
/// for shared use across tasks issuing concurrent multi-edge
/// operations.
pub struct ConnectionPool {
    /// `(node_id, binding_id) → Connection`. `DashMap` lets multiple
    /// tasks pool-lookup concurrently without serializing through a
    /// single mutex.
    entries: DashMap<(u64, BindingId), Connection>,
    /// Edges in drain mode per ADR-042 §3.2.1. Value carries the
    /// instant the edge transitioned to drain so
    /// [`tick_drain_budget`](Self::tick_drain_budget) can enforce
    /// the budget. New `get_or_dial` calls for a draining edge
    /// bypass the entries cache and dial fresh — drain is advisory
    /// for callers already holding a clone of the [`Connection`],
    /// but blocks new work from the pool's lookup path.
    draining: DashMap<(u64, BindingId), Instant>,
    /// Optional override for the gRPC dial URI scheme — `http://`
    /// (default; matches the kiseki-server runtime's plaintext-dev
    /// listener) or `https://` (production mTLS). Tests set
    /// `http://` so they don't need a TLS handshake.
    grpc_uri_scheme: String,
}

impl ConnectionPool {
    /// Build an empty pool. `https` for production; tests / dev mode
    /// override via [`with_grpc_uri_scheme`](Self::with_grpc_uri_scheme).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            draining: DashMap::new(),
            grpc_uri_scheme: "https".into(),
        }
    }

    /// Override the gRPC dial URI scheme. Tests use `http` against
    /// the runtime's plaintext-dev listener.
    #[must_use]
    pub fn with_grpc_uri_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.grpc_uri_scheme = scheme.into();
        self
    }

    /// Look up an existing connection or dial a new one for the
    /// `(node_id, edge.binding_id)` tuple. Subsequent calls for the
    /// same edge return the cached `Connection`.
    ///
    /// # Errors
    /// `PoolError::Grpc` / `PoolError::TcpFramed` on dial failure;
    /// `PoolError::UnsupportedBinding` for ibverbs / libfabric until
    /// phases 9/10 land. `PoolError::UnsupportedFabricAddr` if an
    /// IP binding gets handed a fabric descriptor by mistake.
    pub async fn get_or_dial(
        &self,
        node_id: u64,
        edge: &BindingEndpoint,
    ) -> Result<Connection, PoolError> {
        let key = (node_id, edge.binding_id);
        // Draining edges bypass the cache — new work routes via a
        // fresh dial (typically against a different binding the
        // caller's edge selector already picked). The pool keeps
        // the draining entry alive for in-flight work the caller
        // already started (advisory drain — clones are still
        // dispatchable). Production callers consult
        // [`is_draining`](Self::is_draining) BEFORE `get_or_dial`
        // and select a non-draining binding.
        if !self.draining.contains_key(&key) {
            if let Some(existing) = self.entries.get(&key) {
                return Ok(existing.clone());
            }
        }
        let conn = self.dial(edge).await?;
        // Insert — race-OK because two concurrent dials for the same
        // edge would each succeed and one would replace the other;
        // we'd waste at most one dial. DashMap's `entry` API would
        // serialize but at the cost of holding the shard lock across
        // the await; not worth it.
        if !self.draining.contains_key(&key) {
            self.entries.insert(key, conn.clone());
        }
        Ok(conn)
    }

    /// Mark `(node_id, binding_id)` as draining per ADR-042 §3.2.1.
    /// New work skips this edge; clones already held by callers
    /// continue serving in-flight requests until they complete OR
    /// the drain budget elapses ([`tick_drain_budget`](Self::tick_drain_budget)).
    ///
    /// Idempotent — calling on an already-draining edge does
    /// nothing (the original drain start instant wins).
    pub fn drain_edge(&self, node_id: u64, binding_id: BindingId) {
        self.draining
            .entry((node_id, binding_id))
            .or_insert_with(Instant::now);
    }

    /// Whether `(node_id, binding_id)` is currently draining. The
    /// caller's edge selector consults this before `get_or_dial` to
    /// avoid routing new work to a draining binding.
    #[must_use]
    pub fn is_draining(&self, node_id: u64, binding_id: BindingId) -> bool {
        self.draining.contains_key(&(node_id, binding_id))
    }

    /// Reconcile the pool against a fresh topology snapshot per
    /// ADR-042 §3.2.1's diff trigger. For every pool entry whose
    /// `(node_id, binding_id)` no longer appears in the topology
    /// (binding removed from a node's advertised set, OR node
    /// transitioned to Failed/Evicted), transition the edge to
    /// drain mode.
    ///
    /// Returns the count of newly-drained edges (excludes edges
    /// that were already draining).
    pub fn reconcile_with_topology(&self, snapshot: &Snapshot) -> usize {
        // Build a fast lookup set of currently-advertised
        // (node, binding) pairs.
        let advertised: std::collections::HashSet<(u64, BindingId)> = snapshot
            .nodes
            .iter()
            .flat_map(|node| {
                node.bindings
                    .iter()
                    .map(move |ep| (node.node_id, ep.binding_id))
            })
            .collect();
        let mut newly_drained = 0usize;
        for kv in &self.entries {
            let key = *kv.key();
            if !advertised.contains(&key) && !self.draining.contains_key(&key) {
                self.draining.insert(key, Instant::now());
                newly_drained += 1;
            }
        }
        newly_drained
    }

    /// Hard-close every draining edge whose age exceeds `budget`.
    /// Per ADR-042 §3.2.1: "Past budget, hard-close the connection
    /// per §3.2.2." For TCP-based bindings the kernel handles
    /// cleanup on `Drop`; the pool just removes the entry and
    /// clears the drain flag.
    ///
    /// Returns the count of edges hard-closed this tick.
    pub fn tick_drain_budget(&self, budget: Duration) -> usize {
        let now = Instant::now();
        let expired: Vec<(u64, BindingId)> = self
            .draining
            .iter()
            .filter(|kv| now.duration_since(*kv.value()) >= budget)
            .map(|kv| *kv.key())
            .collect();
        let count = expired.len();
        for key in expired {
            self.entries.remove(&key);
            self.draining.remove(&key);
        }
        count
    }

    /// Borrow the current drain set. Diagnostic — production code
    /// uses [`is_draining`](Self::is_draining) for per-edge checks.
    #[must_use]
    pub fn draining_edges(&self) -> Vec<(u64, BindingId)> {
        self.draining.iter().map(|kv| *kv.key()).collect()
    }

    /// Borrow the pool's current `(node, binding)` set. Used by the
    /// drain-protocol slice to iterate over connections that need
    /// to transition to drain mode after a topology binding-set
    /// change. Cheap snapshot — clones the keys only.
    #[must_use]
    pub fn open_edges(&self) -> Vec<(u64, BindingId)> {
        self.entries.iter().map(|kv| *kv.key()).collect::<Vec<_>>()
    }

    /// Drop the pool entry for `(node_id, binding_id)`. ADR-042
    /// §3.2.2 per-binding hard-close discipline: for TCP-based
    /// bindings (gRPC, TCP-framed), the kernel handles socket
    /// cleanup on `Drop`. RDMA bindings (when added) MUST follow
    /// the ordered cleanup table in §3.2.2 — that's the binding
    /// crate's `Drop` impl; the pool just removes the entry here.
    pub fn drop_edge(&self, node_id: u64, binding_id: BindingId) -> bool {
        self.entries.remove(&(node_id, binding_id)).is_some()
    }

    /// Drop every connection to `node_id`, regardless of binding.
    /// Called when the topology marks the node `Failed` / `Evicted`
    /// (close-on-state-change per §1.7's R3-O1).
    pub fn drop_node(&self, node_id: u64) -> usize {
        let to_remove: Vec<_> = self
            .entries
            .iter()
            .filter(|kv| kv.key().0 == node_id)
            .map(|kv| *kv.key())
            .collect();
        let count = to_remove.len();
        for key in to_remove {
            self.entries.remove(&key);
        }
        count
    }

    /// Number of entries in the pool. Mostly diagnostic.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pool has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    async fn dial(&self, edge: &BindingEndpoint) -> Result<Connection, PoolError> {
        let host_port = match &edge.addr {
            ListenAddr::HostPort(s) => s.clone(),
            ListenAddr::FabricDescriptor(bytes) => {
                return Err(PoolError::UnsupportedFabricAddr(bytes.clone()));
            }
        };
        match edge.binding_id {
            BindingId::Grpc => {
                let uri = format!("{}://{}", self.grpc_uri_scheme, host_port);
                let endpoint = tonic::transport::Channel::from_shared(uri)
                    .map_err(|e| PoolError::Grpc(e.to_string()))?;
                let channel = endpoint
                    .connect()
                    .await
                    .map_err(|e| PoolError::Grpc(e.to_string()))?;
                Ok(Connection::Grpc(channel))
            }
            BindingId::TcpFramed => {
                // v1: plaintext only. TLS plumbing is the same
                // follow-up that's documented for the listener side.
                let client = TcpFramedClient::connect_plaintext(host_port)
                    .await
                    .map_err(|e| PoolError::TcpFramed(e.to_string()))?;
                Ok(Connection::TcpFramed(client))
            }
            BindingId::Ibverbs | BindingId::Libfabric { .. } => {
                Err(PoolError::UnsupportedBinding {
                    binding_id: edge.binding_id,
                })
            }
        }
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_proto::native_contract::LatencyClass;

    fn host_port_endpoint(binding_id: BindingId, addr: &str) -> BindingEndpoint {
        BindingEndpoint {
            binding_id,
            addr: ListenAddr::HostPort(addr.into()),
            latency_class: match binding_id {
                BindingId::Grpc => LatencyClass::Standard,
                BindingId::TcpFramed => LatencyClass::Low,
                _ => LatencyClass::Rdma,
            },
            drain_state: None,
        }
    }

    #[tokio::test]
    async fn unsupported_binding_returns_error_without_caching() {
        let pool = ConnectionPool::new();
        let ibverbs = host_port_endpoint(BindingId::Ibverbs, "10.0.0.1:9000");
        let err = pool
            .get_or_dial(1, &ibverbs)
            .await
            .expect_err("ibverbs unsupported");
        assert!(matches!(
            err,
            PoolError::UnsupportedBinding {
                binding_id: BindingId::Ibverbs
            }
        ));
        // Failed dial does NOT pollute the pool — the next call
        // also takes the same path.
        assert!(pool.is_empty());
    }

    #[tokio::test]
    async fn fabric_descriptor_addr_rejected_for_ip_bindings() {
        let pool = ConnectionPool::new();
        let edge = BindingEndpoint {
            binding_id: BindingId::Grpc,
            addr: ListenAddr::FabricDescriptor(vec![0xCA, 0xFE]),
            latency_class: LatencyClass::Rdma,
            drain_state: None,
        };
        let err = pool
            .get_or_dial(1, &edge)
            .await
            .expect_err("fabric addr unsupported");
        match err {
            PoolError::UnsupportedFabricAddr(bytes) => {
                assert_eq!(bytes, vec![0xCA, 0xFE]);
            }
            other => panic!("expected UnsupportedFabricAddr, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tcp_framed_dial_against_local_loopback_caches_connection() {
        // Spin up a tiny TCP server that accepts a connection but
        // doesn't speak the framed protocol. The TcpFramedClient's
        // connect_plaintext returns Ok as soon as the TCP handshake
        // succeeds; the subsequent reader task just hangs (no
        // frames). That's fine for THIS test — we're checking the
        // pool's caching, not the framed wire.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept loop running in the background, swallowing any
        // accepted streams to keep them open.
        tokio::spawn(async move {
            loop {
                let _ = listener.accept().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let pool = ConnectionPool::new();
        let edge = host_port_endpoint(BindingId::TcpFramed, &addr.to_string());
        let c1 = pool.get_or_dial(7, &edge).await.expect("dial 1");
        assert_eq!(c1.binding_id(), BindingId::TcpFramed);
        assert_eq!(pool.len(), 1);

        // Second call returns the SAME pooled connection — assert
        // by Arc strong-count + entry count unchanged.
        let c2 = pool.get_or_dial(7, &edge).await.expect("dial 2");
        match (&c1, &c2) {
            (Connection::TcpFramed(a), Connection::TcpFramed(b)) => {
                assert!(Arc::ptr_eq(a, b), "second dial must return cached client");
            }
            _ => panic!("both should be TcpFramed"),
        }
        assert_eq!(pool.len(), 1, "pool must not grow on cache hit");
    }

    #[tokio::test]
    async fn distinct_bindings_to_same_node_each_get_their_own_entry() {
        // Spin up TWO loopback listeners (one per binding) on
        // ephemeral ports. Pool caches separately under
        // (node_id, binding_id) keys.
        let l_grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_grpc = l_grpc.local_addr().unwrap();
        let l_tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_tcp = l_tcp.local_addr().unwrap();
        for listener in [l_grpc, l_tcp] {
            tokio::spawn(async move {
                loop {
                    let _ = listener.accept().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
        }
        let pool = ConnectionPool::new().with_grpc_uri_scheme("http");
        let grpc_edge = host_port_endpoint(BindingId::Grpc, &addr_grpc.to_string());
        let tcp_edge = host_port_endpoint(BindingId::TcpFramed, &addr_tcp.to_string());
        let _g = pool.get_or_dial(7, &grpc_edge).await.expect("grpc dial");
        let _t = pool.get_or_dial(7, &tcp_edge).await.expect("tcp dial");
        assert_eq!(pool.len(), 2, "two bindings → two pool entries");
        let mut edges = pool.open_edges();
        edges.sort();
        assert_eq!(edges, vec![(7, BindingId::Grpc), (7, BindingId::TcpFramed)],);
    }

    #[tokio::test]
    async fn drop_edge_removes_only_the_named_entry() {
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();
        for listener in [l1, l2] {
            tokio::spawn(async move {
                loop {
                    let _ = listener.accept().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
        }
        let pool = ConnectionPool::new();
        let _e1 = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &a1.to_string()),
            )
            .await
            .unwrap();
        let _e2 = pool
            .get_or_dial(
                2,
                &host_port_endpoint(BindingId::TcpFramed, &a2.to_string()),
            )
            .await
            .unwrap();
        assert_eq!(pool.len(), 2);
        assert!(pool.drop_edge(1, BindingId::TcpFramed));
        assert_eq!(pool.len(), 1);
        // Idempotent drop returns false the second time.
        assert!(!pool.drop_edge(1, BindingId::TcpFramed));
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn drop_node_clears_every_binding_to_that_node() {
        // §1.7 Failed/Evicted close-on-state-change: every connection
        // to the dead node, across ALL bindings, must drop.
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();
        for listener in [l1, l2] {
            tokio::spawn(async move {
                loop {
                    let _ = listener.accept().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
        }
        let pool = ConnectionPool::new().with_grpc_uri_scheme("http");
        // Same node-id (5), two different bindings.
        let _g = pool
            .get_or_dial(5, &host_port_endpoint(BindingId::Grpc, &a1.to_string()))
            .await
            .unwrap();
        let _t = pool
            .get_or_dial(
                5,
                &host_port_endpoint(BindingId::TcpFramed, &a2.to_string()),
            )
            .await
            .unwrap();
        assert_eq!(pool.len(), 2);
        let dropped = pool.drop_node(5);
        assert_eq!(dropped, 2);
        assert!(pool.is_empty());
    }

    // Note: a `Connection::binding_id()` exhaustiveness test would
    // need to construct each variant; we can't cheaply mint a tonic
    // `Channel` without a server. Match exhaustiveness is enforced
    // by the compiler instead — adding a new `Connection` variant
    // without updating `binding_id()` is a build-time error.

    // -----------------------------------------------------------------
    // Drain protocol (ADR-042 §3.2.1).
    // -----------------------------------------------------------------

    fn snapshot_advertising(version: u64, per_node: Vec<(u64, Vec<BindingId>)>) -> Snapshot {
        let nodes = per_node
            .into_iter()
            .map(|(node_id, ids)| crate::native::Node {
                node_id,
                data_addr: format!("10.0.0.{node_id}:9100"),
                state: kiseki_proto::native_contract::NodeState::Active,
                bindings: ids
                    .into_iter()
                    .map(|id| BindingEndpoint {
                        binding_id: id,
                        addr: ListenAddr::HostPort(format!("10.0.0.{node_id}:9101")),
                        latency_class: match id {
                            BindingId::Grpc => LatencyClass::Standard,
                            BindingId::TcpFramed => LatencyClass::Low,
                            _ => LatencyClass::Rdma,
                        },
                        drain_state: None,
                    })
                    .collect(),
            })
            .collect();
        Snapshot {
            version,
            nodes,
            shards: Vec::new(),
        }
    }

    async fn ephemeral_listener() -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let _ = l.accept().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn drain_edge_marks_advisory_state() {
        let pool = ConnectionPool::new();
        assert!(!pool.is_draining(7, BindingId::Grpc));
        pool.drain_edge(7, BindingId::Grpc);
        assert!(pool.is_draining(7, BindingId::Grpc));
        assert_eq!(pool.draining_edges().len(), 1);
    }

    #[tokio::test]
    async fn drain_edge_is_idempotent_and_preserves_start_instant() {
        let pool = ConnectionPool::new();
        pool.drain_edge(7, BindingId::Grpc);
        let first = pool.draining.get(&(7, BindingId::Grpc)).unwrap().clone();
        // Sleep to ensure a measurable gap.
        tokio::time::sleep(Duration::from_millis(2)).await;
        pool.drain_edge(7, BindingId::Grpc);
        let second = pool.draining.get(&(7, BindingId::Grpc)).unwrap().clone();
        assert_eq!(
            first, second,
            "second drain_edge must NOT reset the original instant; budget timer wins from the first call",
        );
    }

    #[tokio::test]
    async fn reconcile_drains_edges_no_longer_advertised() {
        let pool = ConnectionPool::new();
        let addr = ephemeral_listener().await;
        // Pool entry under (node=1, TcpFramed).
        let _c = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &addr.to_string()),
            )
            .await
            .unwrap();
        assert_eq!(pool.len(), 1);

        // New topology advertises ONLY gRPC for node-1; TCP-framed is
        // gone (e.g. listener crashed or operator downgrade).
        let snap = snapshot_advertising(2, vec![(1, vec![BindingId::Grpc])]);
        let drained = pool.reconcile_with_topology(&snap);
        assert_eq!(drained, 1);
        assert!(pool.is_draining(1, BindingId::TcpFramed));
        // Entry is still in the pool — drain is advisory; in-flight
        // work continues via the cached Connection clone.
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn reconcile_does_not_drain_still_advertised_edges() {
        let pool = ConnectionPool::new();
        let addr = ephemeral_listener().await;
        let _c = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &addr.to_string()),
            )
            .await
            .unwrap();
        // Topology still advertises TCP-framed on node-1.
        let snap = snapshot_advertising(2, vec![(1, vec![BindingId::TcpFramed, BindingId::Grpc])]);
        let drained = pool.reconcile_with_topology(&snap);
        assert_eq!(drained, 0);
        assert!(!pool.is_draining(1, BindingId::TcpFramed));
    }

    #[tokio::test]
    async fn reconcile_drains_every_edge_for_a_disappeared_node() {
        // Two separate nodes with TCP-framed pool entries; the
        // snapshot disappears node-1 entirely. Both edges to node-1
        // (here just one — TcpFramed) drain. We avoid spinning up a
        // real gRPC server in this unit test by testing the
        // disappearance path via TcpFramed bindings on multiple
        // nodes instead. The reconciliation logic doesn't care
        // which binding; it only cares about (node, binding)
        // membership in the snapshot.
        let pool = ConnectionPool::new();
        let a1 = ephemeral_listener().await;
        let a2 = ephemeral_listener().await;
        let _t1 = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &a1.to_string()),
            )
            .await
            .unwrap();
        let _t2 = pool
            .get_or_dial(
                2,
                &host_port_endpoint(BindingId::TcpFramed, &a2.to_string()),
            )
            .await
            .unwrap();
        assert_eq!(pool.len(), 2);
        // Snapshot omits node-1 (Failed/Evicted) but keeps node-2.
        let snap = snapshot_advertising(2, vec![(2, vec![BindingId::TcpFramed])]);
        let drained = pool.reconcile_with_topology(&snap);
        assert_eq!(drained, 1);
        assert!(pool.is_draining(1, BindingId::TcpFramed));
        assert!(!pool.is_draining(2, BindingId::TcpFramed));
    }

    #[tokio::test]
    async fn tick_drain_budget_hard_closes_expired_edges() {
        let pool = ConnectionPool::new();
        let addr = ephemeral_listener().await;
        let _c = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &addr.to_string()),
            )
            .await
            .unwrap();
        pool.drain_edge(1, BindingId::TcpFramed);
        // Tick with a very small budget — beyond the drain age.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let closed = pool.tick_drain_budget(Duration::from_millis(10));
        assert_eq!(closed, 1);
        // Pool entry AND drain marker both gone after hard-close.
        assert!(pool.is_empty());
        assert!(!pool.is_draining(1, BindingId::TcpFramed));
    }

    #[tokio::test]
    async fn tick_drain_budget_keeps_within_budget_edges() {
        let pool = ConnectionPool::new();
        let addr = ephemeral_listener().await;
        let _c = pool
            .get_or_dial(
                1,
                &host_port_endpoint(BindingId::TcpFramed, &addr.to_string()),
            )
            .await
            .unwrap();
        pool.drain_edge(1, BindingId::TcpFramed);
        // Tick with a budget WAY larger than drain age — nothing
        // should hard-close.
        let closed = pool.tick_drain_budget(Duration::from_secs(60));
        assert_eq!(closed, 0);
        assert!(pool.is_draining(1, BindingId::TcpFramed));
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn get_or_dial_on_draining_edge_dials_fresh_without_caching() {
        let pool = ConnectionPool::new();
        let addr = ephemeral_listener().await;
        let edge = host_port_endpoint(BindingId::TcpFramed, &addr.to_string());
        let _c1 = pool.get_or_dial(1, &edge).await.unwrap();
        assert_eq!(pool.len(), 1);
        pool.drain_edge(1, BindingId::TcpFramed);
        // get_or_dial on a draining edge succeeds (we still want to
        // dial fresh for the in-flight-only path), but does NOT
        // overwrite or grow the cache.
        let c2 = pool.get_or_dial(1, &edge).await.unwrap();
        assert_eq!(c2.binding_id(), BindingId::TcpFramed);
        // The cached entry is the original; the freshly-dialed
        // connection is returned to the caller but not cached
        // (cache size unchanged).
        assert_eq!(pool.len(), 1);
        assert!(pool.is_draining(1, BindingId::TcpFramed));
    }

    #[test]
    fn drain_budget_default_pinned() {
        // ADR-042 §3.2.1 default budget — pin so a refactor that
        // changes the constant also breaks the test.
        assert_eq!(DEFAULT_DRAIN_BUDGET_MS, 30_000);
    }
}
