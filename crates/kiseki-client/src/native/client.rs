//! `NativeClient` — gRPC client for `kiseki.v1.native.GatewayDataService`.
//!
//! Phase 5 ships the connection-management surface: dial a node, hold
//! a `tonic::transport::Channel`, expose the [`TopologyCache`] for
//! routing, hand out [`StreamSlot`] guards, and bookkeep active leases
//! via [`LeaseManager`]. The actual streaming Read / Write paths plus
//! cert-binding handshakes are the next slice of work — Phase 5+ adds
//! them once the server side has bridged the POSIX path.
//!
//! The client deliberately mirrors `GatewayOps` ergonomics rather than
//! copying the proto verbs verbatim; the FUSE / FFI / Python wrappers
//! all consume `GatewayOps`, so a `NativeClient: GatewayOps` impl
//! lands here in a future commit (it needs the streaming Read/Write
//! the server side hasn't bridged yet).

use std::sync::Arc;
use std::time::Duration;

use kiseki_common::ids::OrgId;
use kiseki_proto::v1::native::gateway_data_service_client::GatewayDataServiceClient;
use tonic::transport::{Channel, Endpoint};

use super::lease_manager::LeaseManager;
use super::stream_slot::StreamCapMap;
use super::topology_cache::TopologyCache;

/// Errors emitted by the `NativeClient`. Wraps tonic transport errors
/// + lease state + stream-cap exhaustion.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum NativeClientError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("rpc: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("stream cap exhausted for tenant")]
    StreamCapExhausted,
    #[error("lease {lease_id_prefix:?} is fenced or unknown")]
    LeaseUnusable { lease_id_prefix: [u8; 4] },
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// Client-side handle to a single kiseki cluster's data plane.
///
/// `Channel` is lazily dialed (`Endpoint::connect`) and held for the
/// lifetime of the client. For Phase 5 we keep one channel per
/// configured seed; routing-aware multi-leader pools are Phase 5+.
pub struct NativeClient {
    /// Pre-dialed channel to the cluster's data port.
    channel: Channel,
    /// Per-tenant topology cache.
    topology: Arc<TopologyCache>,
    /// Active leases this client holds.
    leases: Arc<LeaseManager>,
    /// In-flight stream counter (RAII via `StreamSlot`).
    stream_caps: Arc<StreamCapMap>,
    /// Per-call default tenant — used to supply `ControlFields.tenant_id`
    /// when the caller doesn't override.
    tenant_id: OrgId,
}

/// Per-seed connect timeout (ADR-008 rev 2 §"Failure modes /
/// mitigation"). Three seeds × 2 s = 6 s worst-case bootstrap latency.
pub const SEED_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Adversary gate-1 finding S1 — cap on leader-hint redirects per
/// request, enforced by callers of the gRPC retry path. A request
/// that has burned through this many leader hints terminates with
/// `LeaderUnavailable` rather than chasing an arbitrarily long
/// chain of stale cache entries.
pub const MAX_LEADER_REDIRECT_HOPS: u8 = 2;

impl NativeClient {
    /// Connect to a single seed address (`host:port` or full URL). The
    /// caller supplies the tenant id — this is what the proto-handler
    /// boundary cross-checks against the SAN-derived tenant on every
    /// RPC.
    ///
    /// Forwarded to [`Self::connect_with_seeds`] with a single-seed
    /// vector for ADR-008 rev-1 back-compat.
    ///
    /// # Errors
    /// Returns `Connect(...)` if the endpoint is malformed or the
    /// initial TCP / TLS handshake fails.
    pub async fn connect(seed: &str, tenant_id: OrgId) -> Result<Self, NativeClientError> {
        Self::connect_with_seeds(&[seed.to_owned()], tenant_id).await
    }

    /// ADR-008 rev 2 — connect with a list of seeds. Dials each seed
    /// in order with a 2 s per-seed connect timeout
    /// (`SEED_CONNECT_TIMEOUT`), falling through to the next on
    /// failure. Returns the first reachable seed's channel.
    ///
    /// # Errors
    /// Returns `Connect("all seeds unreachable: ...")` when every
    /// seed in the list fails to connect within the per-seed timeout.
    pub async fn connect_with_seeds(
        seeds: &[String],
        tenant_id: OrgId,
    ) -> Result<Self, NativeClientError> {
        if seeds.is_empty() {
            return Err(NativeClientError::Connect("no seeds provided".to_owned()));
        }
        let mut last_err: Option<String> = None;
        let mut attempts: Vec<String> = Vec::with_capacity(seeds.len());
        for seed in seeds {
            let url = if seed.starts_with("http://") || seed.starts_with("https://") {
                seed.clone()
            } else {
                format!("http://{seed}")
            };
            let endpoint = match Endpoint::from_shared(url) {
                Ok(ep) => ep
                    .tcp_nodelay(true)
                    .timeout(Duration::from_secs(30))
                    .connect_timeout(SEED_CONNECT_TIMEOUT),
                Err(e) => {
                    attempts.push(seed.clone());
                    last_err = Some(format!("{seed}: malformed: {e}"));
                    continue;
                }
            };
            match endpoint.connect().await {
                Ok(channel) => {
                    return Ok(Self {
                        channel,
                        topology: Arc::new(TopologyCache::new()),
                        leases: LeaseManager::new(),
                        stream_caps: Arc::new(StreamCapMap::new(256)),
                        tenant_id,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        seed = %seed,
                        error = %e,
                        "native client: seed unreachable, falling through to next"
                    );
                    attempts.push(seed.clone());
                    last_err = Some(format!("{seed}: {e}"));
                }
            }
        }
        Err(NativeClientError::Connect(format!(
            "all seeds unreachable ({} attempted: {}); last error: {}",
            attempts.len(),
            attempts.join(", "),
            last_err.unwrap_or_else(|| "<unknown>".to_owned())
        )))
    }

    /// Build a client over a pre-dialed `Channel` (test convenience —
    /// the in-process transport via `tonic::transport::Server` +
    /// `Endpoint::connect_with_connector` doesn't go through DNS).
    #[must_use]
    pub fn from_channel(channel: Channel, tenant_id: OrgId) -> Self {
        Self {
            channel,
            topology: Arc::new(TopologyCache::new()),
            leases: LeaseManager::new(),
            stream_caps: Arc::new(StreamCapMap::new(256)),
            tenant_id,
        }
    }

    /// Override the per-tenant in-flight stream cap.
    #[must_use]
    pub fn with_stream_cap(mut self, cap: usize) -> Self {
        self.stream_caps = Arc::new(StreamCapMap::new(cap));
        self
    }

    /// The default tenant id supplied to every `ControlFields`.
    #[must_use]
    pub fn tenant_id(&self) -> OrgId {
        self.tenant_id
    }

    /// Topology cache snapshot — the FUSE / FFI layers query this for
    /// routing decisions.
    #[must_use]
    pub fn topology(&self) -> &Arc<TopologyCache> {
        &self.topology
    }

    /// Active-lease registry. Background renewal tasks read this.
    #[must_use]
    pub fn leases(&self) -> &Arc<LeaseManager> {
        &self.leases
    }

    /// Stream-cap counter. Tests inspect counts via this handle.
    #[must_use]
    pub fn stream_caps(&self) -> &Arc<StreamCapMap> {
        &self.stream_caps
    }

    /// Build a new gRPC client off the held channel — clone is cheap
    /// (`Channel` is `Clone`). Public so downstream wrappers (FUSE,
    /// Python, FFI) can hold their own typed gRPC handle without
    /// having to dial a fresh channel.
    #[must_use]
    pub fn rpc_client(&self) -> GatewayDataServiceClient<Channel> {
        GatewayDataServiceClient::new(self.channel.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_sane_starting_state() {
        // We can't easily dial a real channel in a unit test; validate
        // the surface that doesn't need a connection.
        let leases = LeaseManager::new();
        assert!(leases.fencing_token([0; 16]).is_none());
        let caps = Arc::new(StreamCapMap::new(8));
        let org = OrgId(uuid::Uuid::nil());
        let _slot = caps.try_acquire(org).unwrap();
        assert_eq!(caps.current(org), 1);
    }

    // Finding S1: MAX_LEADER_REDIRECT_HOPS exists and is 2 per
    // ADR-008 rev 2 §"Failure modes / mitigation".
    #[test]
    fn max_leader_redirect_hops_is_two() {
        assert_eq!(super::MAX_LEADER_REDIRECT_HOPS, 2);
    }

    // Finding S1: SEED_CONNECT_TIMEOUT is the 2 s budget per seed.
    #[test]
    fn seed_connect_timeout_is_two_seconds() {
        assert_eq!(SEED_CONNECT_TIMEOUT.as_secs(), 2);
    }

    // Finding S2: empty seed list errors with a clear message.
    #[tokio::test]
    async fn connect_with_seeds_rejects_empty_list() {
        let result = NativeClient::connect_with_seeds(&[], OrgId(uuid::Uuid::nil())).await;
        let Err(err) = result else {
            panic!("empty seed list must error, got Ok");
        };
        match err {
            NativeClientError::Connect(msg) => {
                assert!(
                    msg.contains("no seeds") || msg.contains("seed"),
                    "expected seed-related error: {msg}"
                );
            }
            other => panic!("expected Connect error, got {other:?}"),
        }
    }

    // Finding S2: all-seeds-unreachable reports each attempted seed.
    #[tokio::test]
    async fn connect_with_seeds_all_unreachable_reports_aggregated_error() {
        // Three guaranteed-unreachable addresses (TEST-NET-1 192.0.2/24
        // per RFC 5737; on a sandboxed test host this returns ECONNREFUSED
        // or a timeout. Use the per-seed 2 s timeout to bound the test.
        let seeds = vec![
            "192.0.2.1:1".to_owned(),
            "192.0.2.2:1".to_owned(),
            "192.0.2.3:1".to_owned(),
        ];
        let started = std::time::Instant::now();
        let result = NativeClient::connect_with_seeds(&seeds, OrgId(uuid::Uuid::nil())).await;
        let Err(err) = result else {
            panic!("unreachable seeds must error, got Ok");
        };
        let elapsed = started.elapsed();
        match err {
            NativeClientError::Connect(msg) => {
                assert!(
                    msg.to_lowercase().contains("all seeds")
                        || msg.to_lowercase().contains("unreachable"),
                    "expected aggregated error mentioning all seeds: {msg}"
                );
            }
            other => panic!("expected Connect error, got {other:?}"),
        }
        // Total time bounded by 3 × per-seed timeout (2 s each) with
        // generous headroom for the timer.
        assert!(
            elapsed < Duration::from_secs(15),
            "connect must bottom out within 3 × 2 s + slack, got {elapsed:?}"
        );
    }
}
