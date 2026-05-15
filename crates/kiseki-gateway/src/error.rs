//! Gateway errors.

use kiseki_common::error::{KisekiError, PermanentError, SecurityError};
use kiseki_common::ids::{NodeId, ShardId};

/// Errors from gateway operations.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Authentication failed (mTLS or protocol-level auth).
    #[error("gateway authentication failed: {0}")]
    AuthenticationFailed(String),

    /// Operation not supported by this protocol.
    #[error("operation not supported: {0}")]
    OperationNotSupported(String),

    /// Protocol-level error (malformed request).
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// Upstream error from the view or composition layer.
    #[error("upstream error: {0}")]
    Upstream(String),

    /// View is stale — watermark too far behind (I-K9).
    #[error("view stale: lag {lag_ms}ms exceeds bound")]
    StaleView {
        /// How far behind the view is (milliseconds).
        lag_ms: u64,
    },

    /// Delta's `hashed_key` is outside the target shard's key range (ADR-033).
    /// Gateway should refresh shard map and retry with the correct shard.
    #[error("key out of range for shard {shard_id:?}")]
    KeyOutOfRange {
        /// The shard that rejected the key.
        shard_id: ShardId,
    },

    /// Write attempted on a read-only namespace. Maps to POSIX EROFS at
    /// the FUSE/POSIX boundary (kiseki-client::fuse_fs).
    #[error("namespace is read-only")]
    ReadOnlyNamespace,

    /// This node is currently unable to resolve the request and the
    /// caller should retry (potentially against a different node).
    /// ADR-040 §D7 + I-2: emitted by the read path when a composition
    /// lookup misses **and** the local persistent hydrator has entered
    /// halt mode (compaction outran us). The S3 gateway maps this to
    /// HTTP 503 with a `Retry-After` header so load balancers route
    /// around the halted node.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// HTTP-style conditional request failed. Maps to S3 `412
    /// Precondition Failed` for `If-None-Match: *` against an existing
    /// key, `If-Match: <etag>` mismatch, or related conditional checks.
    /// The composition store is unchanged; the caller can retry with
    /// different conditions or unconditionally.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    /// Resource (object key, composition) wasn't found. Distinct from
    /// `Upstream` so HTTP layers can map it cleanly to 404.
    #[error("not found: {0}")]
    NotFound(String),

    /// The bucket / namespace named in the request hasn't been
    /// registered. Distinct from [`Self::NotFound`] (object missing
    /// inside a known namespace) so the HTTP layer maps cleanly to
    /// S3 `404 NoSuchBucket` rather than the operationally-opaque
    /// 500 `Upstream("namespace not found: ...")` the gateway
    /// returned previously. The 3rd GCP perf run (2026-05-04) hit
    /// the 500 path when the bench did `PUT /<unregistered-bucket>`
    /// without first calling `PUT /<bucket>`; the upgrade to a typed
    /// 404 makes the operator's mistake obvious.
    #[error("bucket / namespace not registered: {0}")]
    NamespaceNotFound(String),

    /// ADR-044 — the local node is a Raft follower for the target
    /// shard; the leader is on `leader_node_id`. Surfaced only by
    /// the `*_with_forwarding`-suffix gateway entry points
    /// ([`crate::ops::GatewayOps::write_with_forwarding`]). The
    /// native server's proxy fallback (`KISEKI_NATIVE_PROXY_FALLBACK=on`)
    /// matches this variant and dials the leader transparently;
    /// the S3 gateway's 307-redirect path (Step C scope) consumes
    /// the same variant for the `Location:` header.
    #[error("forward to leader: shard={shard_id:?} leader_node_id={leader_node_id:?}")]
    ForwardToLeader {
        /// The shard whose leader is on a different node.
        shard_id: ShardId,
        /// The node id of the actual leader (sourced from
        /// `LogError::ForwardToLeader::leader_node_id`).
        leader_node_id: NodeId,
    },
}

impl From<GatewayError> for KisekiError {
    fn from(e: GatewayError) -> Self {
        match e {
            GatewayError::AuthenticationFailed(_) => {
                KisekiError::Security(SecurityError::AuthenticationFailed)
            }
            _ => KisekiError::Permanent(PermanentError::InvariantViolation(e.to_string())),
        }
    }
}
