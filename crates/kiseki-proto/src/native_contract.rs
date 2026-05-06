//! Native gateway data service — transport-agnostic contract types.
//!
//! ADR-042 §1.7 / §1.8. These are the types every binding (gRPC,
//! TCP-framed-postcard, ibverbs, libfabric/cxi/verbs/sockets) carries
//! verbatim. Bindings serialize them via their preferred codec; the
//! gRPC binding maps via the prost-generated `kiseki.v1.native`
//! messages, the others use postcard directly.
//!
//! `RequestPrincipal` (§1.8) is the binding-agnostic handler-side
//! handshake-context shape: each binding's connection hook stashes the
//! canonical SAN and, at request dispatch time, packages it into a
//! `&dyn RequestPrincipal` that `ServerImpl` reads.
//!
//! `CxiAttestationEnvelope` (§2.4.2) is the application-layer
//! attestation message every cxi connection sends as its first frame.

use std::time::SystemTime;

use kiseki_common::ids::NodeId;
use serde::{Deserialize, Serialize};

/// TCP-framed-postcard wire format for the native binding (ADR-042 §2.2).
pub mod wire_tcp_framed;

/// Per-node connection-establishment latency tier. ADR-042 §1.7.
///
/// Coarse on purpose (§3.6): clients rank
/// `Rdma > Low > Standard` for selection; finer-grained latency
/// signals (queue depth, shard-local hot-path) come from per-shard
/// telemetry, not topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum LatencyClass {
    /// Generic IP networks — gRPC/h2 or TCP-framed over standard NICs.
    Standard,
    /// Tuned IP path — TCP-framed without h2 framing tax.
    Low,
    /// True RDMA fabric — ibverbs or libfabric on cxi/verbs.
    Rdma,
}

/// libfabric provider variant. ADR-042 §2.4.4.
///
/// `Efa` is reserved (deferred per §2.4.3 — AWS-IAM integration is its
/// own ADR) but the variant exists so `BindingId::Libfabric { provider:
/// Efa }` decodes successfully on a server that doesn't ship efa
/// support; the probe self-disqualifies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum LibfabricProvider {
    /// HPE/Cray Slingshot + Cassini.
    Cxi,
    /// InfiniBand verbs via libfabric (alternate path to direct ibverbs
    /// for sites that standardize on libfabric).
    Verbs,
    /// AWS Elastic Fabric Adapter — deferred.
    Efa,
    /// Sockets provider — userspace UDP/TCP, low-perf fallback.
    Sockets,
    /// TCP provider — libfabric over TCP, lowest-perf fallback.
    Tcp,
}

/// Transport binding identifier on a `BindingEndpoint`. ADR-042 §1.7.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum BindingId {
    /// gRPC/h2 over rustls/TCP — universal default, every deployment.
    Grpc,
    /// TCP-framed postcard over rustls/TCP — same TLS trust root,
    /// no h2 framing tax.
    TcpFramed,
    /// Direct InfiniBand verbs / RoCEv2.
    Ibverbs,
    /// libfabric — provider variant carried in payload.
    Libfabric { provider: LibfabricProvider },
}

/// Listener address. ADR-042 §1.7.
///
/// IP bindings carry a UTF-8 `host:port` (gRPC, TCP-framed). Fabric
/// bindings carry an opaque provider-encoded descriptor (cxi, ibverbs,
/// libfabric) — the contract layer treats the bytes as opaque; only
/// the fabric binding's connect/accept code interprets them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ListenAddr {
    /// `host:port` for IP-based bindings.
    HostPort(String),
    /// Provider-opaque descriptor for fabric bindings.
    FabricDescriptor(Vec<u8>),
}

/// Drain coordination state on a `Draining` node. ADR-042 §1.7.1.
///
/// Owned by `kiseki-control` (drain coordinator per ADR-035). Two
/// transition points: drain start (`accepts_new_work=false`,
/// quiesce-window timer starts) → quiesce expiry / all-in-flight-done
/// (`accepts_new_work=true` for `KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS`)
/// → `Evicted`.
///
/// Every transition bumps `topology_version` BEFORE the state is
/// observable from any binding's response trailer (§1.7.1 race-window
/// discipline).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DrainState {
    /// Quiesce window remaining; mirrors §9 lease-tracker view.
    pub quiesce_window_remaining_ms: u64,
    /// `false` during quiesce; briefly `true` during graceful release.
    pub accepts_new_work: bool,
}

/// Cluster-membership state on a `NodeBindings` entry. ADR-042 §1.7.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NodeState {
    /// Healthy, accepting new work.
    Active,
    /// Reduced capacity but still serving (operator alarm signal).
    Degraded,
    /// Draining per ADR-035; clients must consult `DrainState`.
    Draining,
    /// Failed; clients route around (bindings empty per §1.7).
    Failed,
    /// Evicted; clients route around (bindings empty per §1.7).
    Evicted,
}

/// One transport binding listener exposed by a node. ADR-042 §1.7.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEndpoint {
    pub binding_id: BindingId,
    pub addr: ListenAddr,
    pub latency_class: LatencyClass,
    /// `Some` only when the node's `state == Draining`.
    pub drain_state: Option<DrainState>,
}

/// Node + its binding endpoints. ADR-042 §1.7.
///
/// State-vs-bindings invariant (§1.7 table; tested by
/// `valid_state_bindings_pair`): `bindings` is empty iff
/// `state in {Failed, Evicted}`; `drain_state` on any endpoint is
/// `Some(_)` iff `state == Draining`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeBindings {
    pub node_id: NodeId,
    pub state: NodeState,
    pub bindings: Vec<BindingEndpoint>,
}

impl NodeBindings {
    /// Returns true if the §1.7 state-vs-bindings table holds for this
    /// entry. Used by `TopologyCache` validation and contract tests.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        let bindings_empty_required = matches!(self.state, NodeState::Failed | NodeState::Evicted);
        if bindings_empty_required && !self.bindings.is_empty() {
            return false;
        }
        if !bindings_empty_required && self.bindings.is_empty() {
            return false;
        }
        let draining = matches!(self.state, NodeState::Draining);
        for ep in &self.bindings {
            if draining && ep.drain_state.is_none() {
                return false;
            }
            if !draining && ep.drain_state.is_some() {
                return false;
            }
        }
        true
    }
}

/// Per-connection identifier for audit/correlation. ADR-042 §1.8.
///
/// Bindings mint these at accept time; format is binding-defined but
/// MUST be unique per (binding, listener-instance, connection). The
/// contract layer treats it as opaque — comparison is not meaningful
/// across binding boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub u64);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{:016x}", self.0)
    }
}

/// Handler-side per-request principal context. ADR-042 §1.8.
///
/// Each binding's request dispatch entry point packages its stashed
/// canonical SAN into a `RequestPrincipal` impl and passes a reference
/// to `ServerImpl`. Mandate (§1.8): `ServerImpl` reads request-source
/// metadata ONLY through this trait; binding-specific request types
/// (`tonic::Request`, TCP-framed `ConnectionContext`, cxi
/// `AttestationContext`) MUST NOT appear in `kiseki-gateway::native::
/// server`. Enforced by an `arch-check` rule in CI (§1.8).
pub trait RequestPrincipal: Send + Sync {
    /// Canonical SAN URI extracted from the connection's client cert
    /// at handshake (§5 SAN canonicalization). Stable across the
    /// lifetime of the connection.
    fn cert_san_canonical(&self) -> &str;
    /// Which binding accepted the connection. Audit + metrics
    /// attribution.
    fn binding_id(&self) -> BindingId;
    /// Per-connection identifier for audit + correlation.
    fn connection_id(&self) -> ConnectionId;
}

/// Application-layer cxi attestation message. ADR-042 §2.4.2.
///
/// Sent as the FIRST message on every cxi connection, before any other
/// RPC frame. Establishes per-tenant identity (cxi auth-key only
/// validates cluster membership). Validation flow runs in the cxi
/// connection-acceptance hook; failure closes the connection with one
/// of the `Unauthenticated{cxi_*}` reasons.
///
/// Wire format: postcard. Size caps (§2.4.2.1): total body ≤ 16 KiB,
/// `cert_chain_der` total ≤ 8 KiB, `signature` ≤ 128 bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CxiAttestationEnvelope {
    /// Schema version. Server rejects > 1 with
    /// `Unauthenticated{cxi_attestation_schema_too_new}` (§2.4.2 step
    /// 1) — fail-closed against newer clients on older binaries.
    pub schema_version: u8,
    /// Full x.509 chain (DER-encoded). `[0]` is the leaf used for
    /// signature verify; rest provide chain-to-CA validation. Total
    /// size cap 8 KiB (§2.4.2.1).
    pub cert_chain_der: Vec<Vec<u8>>,
    /// Canonical SAN URI from `cert_chain_der[0]`. MUST byte-equal the
    /// server's own canonicalization of the leaf cert (§2.4.2 step 4).
    pub canonical_san: String,
    /// Replay-window timestamp. Validated within ±30 s of server HLC
    /// (§2.4.2 step 5).
    pub issued_at: SystemTime,
    /// CSPRNG nonce, per attestation. Per-(SAN, nonce) bloom 60 s
    /// rejects duplicates (§2.4.2 step 7).
    pub nonce: [u8; 32],
    /// ECDSA-P256 signature over the canonical message (§2.4.2):
    /// `b"kiseki/cxi-attestation/v1" || schema_version ||
    /// canonical_san_bytes || issued_at_be8 || nonce`. Size cap 128
    /// bytes (§2.4.2.1).
    pub signature: Vec<u8>,
}

/// Transport-agnostic error taxonomy for the native gateway data
/// service. ADR-042 §1.4. Bindings map each variant onto their
/// wire-level error mechanism (gRPC `tonic::Status`, TCP-framed
/// status byte, ibverbs reject); the variant + reason string IS the
/// canonical signal — bindings preserve both.
///
/// 12 variants per §1.4. `NotLeader` carries the leader node id so
/// clients redirect without a topology refresh round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum NativeError {
    /// mTLS / SAN / token / cxi-attestation failure.
    #[error("unauthenticated: {reason}")]
    Unauthenticated {
        /// Free-form reason; bindings preserve verbatim.
        reason: String,
    },
    /// Tenant mismatch, ACL, SAN-vs-payload tenant mismatch.
    #[error("permission denied: {reason}")]
    PermissionDenied {
        /// Free-form reason.
        reason: String,
    },
    /// Malformed payload, missing required field.
    #[error("invalid argument: {reason}")]
    InvalidArgument {
        /// Free-form reason.
        reason: String,
    },
    /// Namespace / inode / composition / object not found.
    #[error("not found: {what}")]
    NotFound {
        /// What wasn't found (resource kind + identifier).
        what: String,
    },
    /// `If-None-Match: *` conflict on existing key.
    #[error("already exists: {what}")]
    AlreadyExists {
        /// Resource that already exists.
        what: String,
    },
    /// Conditional check rejected (`If-Match`, lease-fenced write).
    #[error("precondition failed: {reason}")]
    PreconditionFailed {
        /// What precondition failed.
        reason: String,
    },
    /// Stream cap, byte range out of bounds.
    #[error("out of range: {reason}")]
    OutOfRange {
        /// What range was exceeded.
        reason: String,
    },
    /// Tenant stream cap, dedup table cap, cxi attestation
    /// rate-limit / source-cooldown / verify-queue-full / oversize.
    #[error("resource exhausted: {reason}")]
    ResourceExhausted {
        /// What was exhausted.
        reason: String,
    },
    /// Partial-chunk-failure, lease fenced, multipart abort.
    #[error("aborted: {reason}")]
    Aborted {
        /// Why aborted.
        reason: String,
    },
    /// Node draining, leader unknown.
    #[error("unavailable: {reason}")]
    Unavailable {
        /// Why unavailable.
        reason: String,
    },
    /// Wrong leader; client should redirect to `leader_node_id`. Maps
    /// to gRPC `failed_precondition` with leader-id metadata; on
    /// TCP-framed it's a status byte 0x1A with the node id in
    /// payload.
    #[error("not leader: redirect to node {leader_node_id:?}")]
    NotLeader {
        /// The node id the client should redirect to. `None` if the
        /// server is itself uncertain — client must do a topology
        /// refresh.
        leader_node_id: Option<NodeId>,
    },
    /// Unhandled bug; bindings emit a redacted reason — full reason
    /// is server-side log only, not propagated to clients.
    #[error("internal: {reason}")]
    Internal {
        /// Server-side reason (not exposed to client by safe bindings).
        reason: String,
    },
}

impl NativeError {
    /// Stable identifier per variant. Audit + metrics labels key off
    /// this, NOT the `Display` form. Renaming a tag is a breaking
    /// change for SOC dashboards.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Unauthenticated { .. } => "unauthenticated",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyExists { .. } => "already_exists",
            Self::PreconditionFailed { .. } => "precondition_failed",
            Self::OutOfRange { .. } => "out_of_range",
            Self::ResourceExhausted { .. } => "resource_exhausted",
            Self::Aborted { .. } => "aborted",
            Self::Unavailable { .. } => "unavailable",
            Self::NotLeader { .. } => "not_leader",
            Self::Internal { .. } => "internal",
        }
    }
}

/// Domain-separator literal for the cxi attestation signature
/// (§2.4.2). Stable, exported so binding code and tests share the
/// exact bytes.
pub const CXI_ATTESTATION_SIG_DOMAIN: &[u8] = b"kiseki/cxi-attestation/v1";

/// Current `CxiAttestationEnvelope` schema version. Server rejects
/// envelopes whose `schema_version > CXI_ATTESTATION_SCHEMA_VERSION`
/// with `cxi_attestation_schema_too_new`.
pub const CXI_ATTESTATION_SCHEMA_VERSION: u8 = 1;

impl CxiAttestationEnvelope {
    /// Deterministically build the canonical message bytes that the
    /// signature covers (§2.4.2 "Canonical message signed"). Identical
    /// on signer and verifier; any mismatch in this construction
    /// breaks every cxi connection — high-leverage code, kept as one
    /// callable so signer + verifier can never drift.
    #[must_use]
    pub fn canonical_message(&self) -> Vec<u8> {
        Self::build_canonical_message(
            self.schema_version,
            self.canonical_san.as_bytes(),
            self.issued_at,
            &self.nonce,
        )
    }

    /// Same construction, callable before the envelope struct exists
    /// (signer side, where signature is the last thing computed).
    #[must_use]
    pub fn build_canonical_message(
        schema_version: u8,
        canonical_san_bytes: &[u8],
        issued_at: SystemTime,
        nonce: &[u8; 32],
    ) -> Vec<u8> {
        let issued_at_ms = issued_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut buf = Vec::with_capacity(
            CXI_ATTESTATION_SIG_DOMAIN.len() + 1 + canonical_san_bytes.len() + 8 + 32,
        );
        buf.extend_from_slice(CXI_ATTESTATION_SIG_DOMAIN);
        buf.push(schema_version);
        buf.extend_from_slice(canonical_san_bytes);
        buf.extend_from_slice(&issued_at_ms.to_be_bytes());
        buf.extend_from_slice(nonce);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_port_endpoint(binding_id: BindingId, host: &str) -> BindingEndpoint {
        BindingEndpoint {
            binding_id,
            addr: ListenAddr::HostPort(host.into()),
            latency_class: LatencyClass::Standard,
            drain_state: None,
        }
    }

    #[test]
    fn binding_id_postcard_roundtrip_grpc() {
        let id = BindingId::Grpc;
        let bytes = postcard::to_allocvec(&id).expect("encode");
        let decoded: BindingId = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(id, decoded);
    }

    #[test]
    fn binding_id_postcard_roundtrip_libfabric_cxi() {
        let id = BindingId::Libfabric {
            provider: LibfabricProvider::Cxi,
        };
        let bytes = postcard::to_allocvec(&id).expect("encode");
        let decoded: BindingId = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(id, decoded);
    }

    #[test]
    fn binding_id_all_variants_postcard_roundtrip() {
        for id in [
            BindingId::Grpc,
            BindingId::TcpFramed,
            BindingId::Ibverbs,
            BindingId::Libfabric {
                provider: LibfabricProvider::Cxi,
            },
            BindingId::Libfabric {
                provider: LibfabricProvider::Verbs,
            },
            BindingId::Libfabric {
                provider: LibfabricProvider::Efa,
            },
            BindingId::Libfabric {
                provider: LibfabricProvider::Sockets,
            },
            BindingId::Libfabric {
                provider: LibfabricProvider::Tcp,
            },
        ] {
            let bytes = postcard::to_allocvec(&id).expect("encode");
            let decoded: BindingId = postcard::from_bytes(&bytes).expect("decode");
            assert_eq!(id, decoded);
        }
    }

    #[test]
    fn drain_state_postcard_roundtrip() {
        let ds = DrainState {
            quiesce_window_remaining_ms: 12_345,
            accepts_new_work: false,
        };
        let bytes = postcard::to_allocvec(&ds).expect("encode");
        let decoded: DrainState = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(ds, decoded);
    }

    #[test]
    fn binding_endpoint_with_drain_state_roundtrip() {
        let ep = BindingEndpoint {
            binding_id: BindingId::TcpFramed,
            addr: ListenAddr::HostPort("10.0.0.1:7000".into()),
            latency_class: LatencyClass::Low,
            drain_state: Some(DrainState {
                quiesce_window_remaining_ms: 5_000,
                accepts_new_work: true,
            }),
        };
        let bytes = postcard::to_allocvec(&ep).expect("encode");
        let decoded: BindingEndpoint = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(ep, decoded);
    }

    #[test]
    fn fabric_descriptor_endpoint_roundtrip() {
        let ep = BindingEndpoint {
            binding_id: BindingId::Libfabric {
                provider: LibfabricProvider::Cxi,
            },
            addr: ListenAddr::FabricDescriptor(vec![0xCA, 0xFE, 0xBA, 0xBE]),
            latency_class: LatencyClass::Rdma,
            drain_state: None,
        };
        let bytes = postcard::to_allocvec(&ep).expect("encode");
        let decoded: BindingEndpoint = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(ep, decoded);
    }

    #[test]
    fn node_bindings_active_is_consistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Active,
            bindings: vec![host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000")],
        };
        assert!(nb.is_consistent());
    }

    #[test]
    fn node_bindings_active_with_empty_bindings_is_inconsistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Active,
            bindings: vec![],
        };
        assert!(!nb.is_consistent());
    }

    #[test]
    fn node_bindings_failed_with_bindings_is_inconsistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Failed,
            bindings: vec![host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000")],
        };
        assert!(!nb.is_consistent());
    }

    #[test]
    fn node_bindings_failed_empty_is_consistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Failed,
            bindings: vec![],
        };
        assert!(nb.is_consistent());
    }

    #[test]
    fn node_bindings_evicted_empty_is_consistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Evicted,
            bindings: vec![],
        };
        assert!(nb.is_consistent());
    }

    #[test]
    fn node_bindings_draining_requires_drain_state_on_every_endpoint() {
        let with = BindingEndpoint {
            drain_state: Some(DrainState {
                quiesce_window_remaining_ms: 1000,
                accepts_new_work: false,
            }),
            ..host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000")
        };
        let without = host_port_endpoint(BindingId::TcpFramed, "10.0.0.1:7001");
        let mixed = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Draining,
            bindings: vec![with, without],
        };
        assert!(
            !mixed.is_consistent(),
            "Draining with any endpoint missing drain_state must be flagged inconsistent",
        );
    }

    #[test]
    fn node_bindings_draining_with_drain_state_on_all_is_consistent() {
        let drain_state = Some(DrainState {
            quiesce_window_remaining_ms: 1000,
            accepts_new_work: false,
        });
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Draining,
            bindings: vec![
                BindingEndpoint {
                    drain_state,
                    ..host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000")
                },
                BindingEndpoint {
                    drain_state,
                    ..host_port_endpoint(BindingId::TcpFramed, "10.0.0.1:7001")
                },
            ],
        };
        assert!(nb.is_consistent());
    }

    #[test]
    fn node_bindings_active_with_drain_state_is_inconsistent() {
        let nb = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Active,
            bindings: vec![BindingEndpoint {
                drain_state: Some(DrainState {
                    quiesce_window_remaining_ms: 0,
                    accepts_new_work: true,
                }),
                ..host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000")
            }],
        };
        assert!(
            !nb.is_consistent(),
            "drain_state may only be Some when state == Draining",
        );
    }

    #[test]
    fn node_bindings_postcard_roundtrip() {
        let nb = NodeBindings {
            node_id: NodeId(42),
            state: NodeState::Active,
            bindings: vec![
                host_port_endpoint(BindingId::Grpc, "10.0.0.1:7000"),
                host_port_endpoint(BindingId::TcpFramed, "10.0.0.1:7001"),
            ],
        };
        let bytes = postcard::to_allocvec(&nb).expect("encode");
        let decoded: NodeBindings = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(nb, decoded);
    }

    #[test]
    fn connection_id_display_is_stable_hex() {
        assert_eq!(
            ConnectionId(0xDEAD_BEEF_CAFE_F00D).to_string(),
            "conn-deadbeefcafef00d"
        );
        assert_eq!(ConnectionId(0).to_string(), "conn-0000000000000000");
    }

    #[test]
    fn connection_id_postcard_roundtrip() {
        let id = ConnectionId(0x1234_5678_9ABC_DEF0);
        let bytes = postcard::to_allocvec(&id).expect("encode");
        let decoded: ConnectionId = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(id, decoded);
    }

    /// Per ADR-042 §1.8: round-trip a `RequestPrincipal` per binding,
    /// asserting the same SAN survives. Uses a minimal stub
    /// implementation; binding-specific tests live in each binding's
    /// crate.
    struct PrincipalStub {
        san: String,
        binding: BindingId,
        conn: ConnectionId,
    }
    impl RequestPrincipal for PrincipalStub {
        fn cert_san_canonical(&self) -> &str {
            &self.san
        }
        fn binding_id(&self) -> BindingId {
            self.binding
        }
        fn connection_id(&self) -> ConnectionId {
            self.conn
        }
    }

    fn read_via_trait(p: &dyn RequestPrincipal) -> (String, BindingId, ConnectionId) {
        (
            p.cert_san_canonical().to_string(),
            p.binding_id(),
            p.connection_id(),
        )
    }

    #[test]
    fn request_principal_san_survives_through_dyn_dispatch() {
        let want_san = "spiffe://kiseki/tenant/org-perf/workload/foo";
        for binding in [
            BindingId::Grpc,
            BindingId::TcpFramed,
            BindingId::Ibverbs,
            BindingId::Libfabric {
                provider: LibfabricProvider::Cxi,
            },
        ] {
            let stub = PrincipalStub {
                san: want_san.into(),
                binding,
                conn: ConnectionId(1),
            };
            let (got_san, got_binding, got_conn) = read_via_trait(&stub);
            assert_eq!(got_san, want_san, "binding={binding:?}");
            assert_eq!(got_binding, binding);
            assert_eq!(got_conn, ConnectionId(1));
        }
    }

    #[test]
    fn cxi_attestation_envelope_postcard_roundtrip() {
        let env = CxiAttestationEnvelope {
            schema_version: CXI_ATTESTATION_SCHEMA_VERSION,
            cert_chain_der: vec![vec![0x30, 0x82], vec![0x30, 0x83]],
            canonical_san: "spiffe://kiseki/tenant/org-perf/workload/foo".into(),
            issued_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000),
            nonce: [0x77; 32],
            signature: vec![0xAB; 64],
        };
        let bytes = postcard::to_allocvec(&env).expect("encode");
        let decoded: CxiAttestationEnvelope = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(env, decoded);
    }

    /// Domain-separator drift is the highest-leverage break in cxi —
    /// any mismatch here invalidates every legitimate connection. Pin
    /// the literal so a refactor that reformats the string also
    /// breaks the test.
    #[test]
    fn cxi_attestation_domain_separator_is_pinned() {
        assert_eq!(CXI_ATTESTATION_SIG_DOMAIN, b"kiseki/cxi-attestation/v1");
    }

    #[test]
    fn cxi_attestation_canonical_message_is_deterministic() {
        let env = CxiAttestationEnvelope {
            schema_version: 1,
            cert_chain_der: vec![],
            canonical_san: "spiffe://kiseki/tenant/org-test/workload/x".into(),
            issued_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000),
            nonce: [0x42; 32],
            signature: vec![],
        };
        let a = env.canonical_message();
        let b = env.canonical_message();
        assert_eq!(a, b, "canonical_message must be deterministic");
        // Builder form (signer side) must produce the same bytes.
        let c = CxiAttestationEnvelope::build_canonical_message(
            env.schema_version,
            env.canonical_san.as_bytes(),
            env.issued_at,
            &env.nonce,
        );
        assert_eq!(a, c, "instance and builder forms must match");
    }

    #[test]
    fn cxi_attestation_canonical_message_layout() {
        let san = b"spiffe://kiseki/test";
        let nonce = [0x11; 32];
        let issued_at =
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(0x0102_0304_0506_0708);
        let bytes = CxiAttestationEnvelope::build_canonical_message(1, san, issued_at, &nonce);
        // Expected layout: domain || version || san || issued_at_be8 || nonce.
        let mut want = Vec::new();
        want.extend_from_slice(b"kiseki/cxi-attestation/v1");
        want.push(1);
        want.extend_from_slice(san);
        want.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        want.extend_from_slice(&nonce);
        assert_eq!(bytes, want);
    }

    /// Differential-fuzz-by-hand: the canonical message MUST change
    /// when any covered field changes — otherwise an attacker can
    /// substitute (san, version, time, nonce) without invalidating the
    /// signature.
    #[test]
    fn cxi_attestation_canonical_message_changes_on_any_field() {
        let base = CxiAttestationEnvelope::build_canonical_message(
            1,
            b"san-a",
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1000),
            &[0; 32],
        );
        let other_version = CxiAttestationEnvelope::build_canonical_message(
            2,
            b"san-a",
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1000),
            &[0; 32],
        );
        let other_san = CxiAttestationEnvelope::build_canonical_message(
            1,
            b"san-b",
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1000),
            &[0; 32],
        );
        let other_time = CxiAttestationEnvelope::build_canonical_message(
            1,
            b"san-a",
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1001),
            &[0; 32],
        );
        let other_nonce = CxiAttestationEnvelope::build_canonical_message(
            1,
            b"san-a",
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1000),
            &[1; 32],
        );
        assert_ne!(base, other_version);
        assert_ne!(base, other_san);
        assert_ne!(base, other_time);
        assert_ne!(base, other_nonce);
    }

    #[test]
    fn schema_version_pinned_at_one() {
        assert_eq!(CXI_ATTESTATION_SCHEMA_VERSION, 1);
    }

    #[test]
    fn native_error_postcard_roundtrip_simple_variant() {
        let e = NativeError::Unauthenticated {
            reason: "tls_info_missing".into(),
        };
        let bytes = postcard::to_allocvec(&e).expect("encode");
        let decoded: NativeError = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(e, decoded);
    }

    #[test]
    fn native_error_postcard_roundtrip_not_leader() {
        let e = NativeError::NotLeader {
            leader_node_id: Some(NodeId(7)),
        };
        let bytes = postcard::to_allocvec(&e).expect("encode");
        let decoded: NativeError = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(e, decoded);
        let unknown = NativeError::NotLeader {
            leader_node_id: None,
        };
        let bytes2 = postcard::to_allocvec(&unknown).expect("encode");
        let decoded2: NativeError = postcard::from_bytes(&bytes2).expect("decode");
        assert_eq!(unknown, decoded2);
    }

    #[test]
    fn native_error_tag_pinned_for_every_variant() {
        // SOC dashboards key off these strings — pin them so a refactor
        // that renames a variant also breaks the test (forcing a
        // conscious dashboard-rename decision).
        let cases: Vec<(NativeError, &'static str)> = vec![
            (
                NativeError::Unauthenticated { reason: "x".into() },
                "unauthenticated",
            ),
            (
                NativeError::PermissionDenied { reason: "x".into() },
                "permission_denied",
            ),
            (
                NativeError::InvalidArgument { reason: "x".into() },
                "invalid_argument",
            ),
            (NativeError::NotFound { what: "x".into() }, "not_found"),
            (
                NativeError::AlreadyExists { what: "x".into() },
                "already_exists",
            ),
            (
                NativeError::PreconditionFailed { reason: "x".into() },
                "precondition_failed",
            ),
            (
                NativeError::OutOfRange { reason: "x".into() },
                "out_of_range",
            ),
            (
                NativeError::ResourceExhausted { reason: "x".into() },
                "resource_exhausted",
            ),
            (NativeError::Aborted { reason: "x".into() }, "aborted"),
            (
                NativeError::Unavailable { reason: "x".into() },
                "unavailable",
            ),
            (
                NativeError::NotLeader {
                    leader_node_id: Some(NodeId(1)),
                },
                "not_leader",
            ),
            (NativeError::Internal { reason: "x".into() }, "internal"),
        ];
        // 12 variants per §1.4.
        assert_eq!(cases.len(), 12);
        for (err, want_tag) in cases {
            assert_eq!(err.tag(), want_tag, "{err:?}");
        }
    }

    #[test]
    fn native_error_display_includes_reason() {
        // §1.4 mandate: "the variant + reason string IS the canonical
        // signal" — so the Display form must surface the reason verbatim
        // (bindings preserve both for client visibility).
        let e = NativeError::PermissionDenied {
            reason: "san_payload_tenant_mismatch".into(),
        };
        let shown = e.to_string();
        assert!(
            shown.contains("san_payload_tenant_mismatch"),
            "Display lost the reason: {shown}",
        );
    }
}
