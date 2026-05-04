//! gRPC `ClusterChunkService` client wrapper that satisfies
//! [`FabricPeer`].
//!
//! Phase 16a step 6. One [`GrpcFabricPeer`] per cluster peer.
//! Each holds a tonic [`Channel`] (which already does
//! auto-reconnect under the hood) plus a typed
//! [`ClusterChunkServiceClient`] handle. Calls are wrapped with a
//! single in-line retry on transient errors so a momentary
//! connection blip doesn't propagate to the [`ClusteredChunkStore`]
//! quorum gate.
//!
//! Retry policy (16a, deliberately minimal):
//! - Retry exactly once on `Status::Unavailable` (gRPC transient).
//! - 100 ms backoff before the retry attempt.
//! - Do **not** retry `NotFound` — it's a real signal driving the
//!   read-side fabric ladder.
//! - Do **not** retry `PermissionDenied` — that's the SAN
//!   interceptor rejecting us; retrying won't help.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1 as pb;
use kiseki_proto::v1::cluster_chunk_service_client::ClusterChunkServiceClient;
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::metrics::{op as op_label, outcome, FabricMetrics};
use crate::peer::{FabricPeer, FabricPeerError};

/// Backoff before the single retry attempt.
const RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Real-network [`FabricPeer`] impl talking to a peer's
/// `ClusterChunkService` endpoint over mTLS.
pub struct GrpcFabricPeer {
    name: String,
    client: ClusterChunkServiceClient<Channel>,
    metrics: Option<Arc<FabricMetrics>>,
}

/// Application contract: maximum ciphertext bytes per envelope.
/// The gateway's chunking policy MUST keep envelopes at or below
/// this size — anything larger is split into multiple chunks
/// before fan-out.
pub const FABRIC_CIPHERTEXT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Wrapper overhead headroom: prost field tags + length varints
/// for the envelope's `chunk_id` (32 B), `auth_tag` (16 B),
/// `nonce` (12 B), `system_epoch` + `tenant_epoch` varints, and
/// the outer `PutFragmentRequest` fields (`chunk_id`,
/// `fragment_index`, `org_id`, `pool_id`). Measured at ~120 bytes
/// for the current schema; bumped to 64 KiB for safety so a
/// future field add can't quietly push us back into the
/// 2026-05-04 "h2 protocol error: http2 error" failure mode
/// (see `tests/grpc_max_message_size.rs`).
pub const FABRIC_WRAPPER_HEADROOM_BYTES: usize = 64 * 1024;

/// Per-RPC message size cap on the cluster fabric — both
/// directions.
///
/// Sized as `FABRIC_CIPHERTEXT_MAX_BYTES + FABRIC_WRAPPER_HEADROOM_BYTES`
/// so a near-cap envelope round-trips through tonic without
/// hitting the underlying h2 `RST_STREAM`.
///
/// History: 2026-05-04 GCP transport-profile run discovered this
/// constant was sized at exactly 256 MiB with no headroom — every
/// 256 MB S3 PUT failed with `quorum lost: only 1/2 replicas acked`
/// because the leader's `PutFragment` fan-out hit the receiver-
/// side decoder cap. tonic returned `Status::resource_exhausted` →
/// h2 emitted `RST_STREAM` with `INTERNAL_ERROR` → the leader's
/// transport layer surfaced "h2 protocol error: http2 error".
/// Tonic's default of 4 MiB is far below kiseki's chunk sizes;
/// the original sizing was correct in spirit (256 MiB ciphertext
/// cap) but failed to account for the wrapper bytes.
///
/// Tonic defaults to 4 MiB for both decoding and encoding sizes;
/// kiseki replaces both with this constant on every fabric
/// `Channel` (`GrpcFabricPeer::new`) and `Server` (`runtime`).
pub const FABRIC_MAX_MESSAGE_BYTES: usize =
    FABRIC_CIPHERTEXT_MAX_BYTES + FABRIC_WRAPPER_HEADROOM_BYTES;

impl GrpcFabricPeer {
    /// Build a fabric peer from a connected tonic channel + a
    /// human-readable name (typically the peer's node id).
    #[must_use]
    pub fn new(name: impl Into<String>, channel: Channel) -> Self {
        Self {
            name: name.into(),
            client: ClusterChunkServiceClient::new(channel)
                .max_decoding_message_size(FABRIC_MAX_MESSAGE_BYTES)
                .max_encoding_message_size(FABRIC_MAX_MESSAGE_BYTES),
            metrics: None,
        }
    }

    /// Attach metrics — every RPC will record an outcome + duration.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<FabricMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn record(&self, op: &str, started: Instant, outcome: &str) {
        if let Some(m) = self.metrics.as_ref() {
            m.record_op(op, &self.name, outcome, started.elapsed());
        }
    }

    fn outcome_for(err: &FabricPeerError) -> &'static str {
        match err {
            FabricPeerError::NotFound => outcome::NOT_FOUND,
            FabricPeerError::Unavailable(_) => outcome::UNAVAILABLE,
            FabricPeerError::Rejected(_) => outcome::REJECTED,
            FabricPeerError::Transport(_) => outcome::TRANSPORT,
        }
    }

    fn rust_envelope_to_proto(e: &Envelope) -> pb::Envelope {
        pb::Envelope {
            ciphertext: e.ciphertext.clone(),
            auth_tag: e.auth_tag.to_vec(),
            nonce: e.nonce.to_vec(),
            algorithm: pb::EncryptionAlgorithm::Aes256Gcm as i32,
            system_epoch: Some(pb::KeyEpoch {
                value: e.system_epoch.0,
            }),
            tenant_epoch: e.tenant_epoch.map(|k| pb::KeyEpoch { value: k.0 }),
            tenant_wrapped_material: e.tenant_wrapped_material.clone().unwrap_or_default(),
            chunk_id: Some(pb::ChunkId {
                value: e.chunk_id.0.to_vec(),
            }),
        }
    }

    fn proto_envelope_to_rust(p: pb::Envelope) -> Result<Envelope, FabricPeerError> {
        if p.auth_tag.len() != 16 {
            return Err(FabricPeerError::Transport(
                "auth_tag must be 16 bytes".into(),
            ));
        }
        if p.nonce.len() != 12 {
            return Err(FabricPeerError::Transport("nonce must be 12 bytes".into()));
        }
        let cid = p
            .chunk_id
            .ok_or_else(|| FabricPeerError::Transport("envelope missing chunk_id".into()))?;
        if cid.value.len() != 32 {
            return Err(FabricPeerError::Transport(
                "chunk_id must be 32 bytes".into(),
            ));
        }
        let sys_epoch = p
            .system_epoch
            .ok_or_else(|| FabricPeerError::Transport("system_epoch missing".into()))?;

        let mut auth_tag = [0u8; 16];
        auth_tag.copy_from_slice(&p.auth_tag);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&p.nonce);
        let mut chunk_arr = [0u8; 32];
        chunk_arr.copy_from_slice(&cid.value);

        let tenant_wrapped_material = if p.tenant_wrapped_material.is_empty() {
            None
        } else {
            Some(p.tenant_wrapped_material)
        };

        Ok(Envelope {
            ciphertext: p.ciphertext,
            auth_tag,
            nonce,
            system_epoch: kiseki_common::tenancy::KeyEpoch(sys_epoch.value),
            tenant_epoch: p
                .tenant_epoch
                .map(|e| kiseki_common::tenancy::KeyEpoch(e.value)),
            tenant_wrapped_material,
            chunk_id: ChunkId(chunk_arr),
        })
    }
}

/// Map a tonic [`Status`] onto our [`FabricPeerError`] taxonomy.
#[must_use]
pub fn status_to_fabric_err(s: &Status) -> FabricPeerError {
    match s.code() {
        Code::NotFound => FabricPeerError::NotFound,
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
            FabricPeerError::Unavailable(s.message().to_owned())
        }
        Code::PermissionDenied | Code::Unauthenticated => {
            FabricPeerError::Rejected(s.message().to_owned())
        }
        _ => FabricPeerError::Transport(format!("{}: {}", s.code(), s.message())),
    }
}

/// Returns true iff a gRPC status warrants the single retry attempt.
#[must_use]
pub fn is_retriable_status(s: &Status) -> bool {
    matches!(
        s.code(),
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled
    )
}

/// Run `op` once; on a retriable status, sleep and retry exactly
/// once. Mirrors the docstring's policy table.
async fn with_retry<F, Fut, T>(mut op: F) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Status>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(s) if is_retriable_status(&s) => {
            tokio::time::sleep(RETRY_BACKOFF).await;
            op().await
        }
        Err(s) => Err(s),
    }
}

#[async_trait]
impl FabricPeer for GrpcFabricPeer {
    fn name(&self) -> &str {
        &self.name
    }

    async fn put_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
        pool_id: String,
        envelope: Envelope,
    ) -> Result<bool, FabricPeerError> {
        let started = Instant::now();
        let proto_encode_started = Instant::now();
        let env_proto = Self::rust_envelope_to_proto(&envelope);
        if let Some(m) = self.metrics.as_ref() {
            m.observe_put_send("proto_encode", proto_encode_started.elapsed());
        }
        let transport_started = Instant::now();
        let result = with_retry(|| {
            let mut client = self.client.clone();
            let req = pb::PutFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index,
                tenant_id: Some(pb::OrgId {
                    value: tenant_id.0.to_string(),
                }),
                pool_id: Some(pb::AffinityPoolId {
                    value: pool_id.as_bytes().to_vec(),
                }),
                envelope: Some(env_proto.clone()),
                leader_ts: None,
            };
            async move { client.put_fragment(req).await }
        })
        .await
        .map_err(|s| status_to_fabric_err(&s));
        if let Some(m) = self.metrics.as_ref() {
            m.observe_put_send("transport", transport_started.elapsed());
        }

        match result {
            Ok(resp) => {
                self.record(op_label::PUT, started, outcome::OK);
                Ok(resp.into_inner().stored)
            }
            Err(e) => {
                self.record(op_label::PUT, started, Self::outcome_for(&e));
                Err(e)
            }
        }
    }

    async fn get_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<Envelope, FabricPeerError> {
        let started = Instant::now();
        let result = with_retry(|| {
            let mut client = self.client.clone();
            let req = pb::GetFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index,
            };
            async move { client.get_fragment(req).await }
        })
        .await
        .map_err(|s| status_to_fabric_err(&s));

        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                self.record(op_label::GET, started, Self::outcome_for(&e));
                return Err(e);
            }
        };
        let env = match resp
            .into_inner()
            .envelope
            .ok_or_else(|| FabricPeerError::Transport("response missing envelope".into()))
        {
            Ok(e) => e,
            Err(e) => {
                self.record(op_label::GET, started, Self::outcome_for(&e));
                return Err(e);
            }
        };
        match Self::proto_envelope_to_rust(env) {
            Ok(env) => {
                self.record(op_label::GET, started, outcome::OK);
                Ok(env)
            }
            Err(e) => {
                self.record(op_label::GET, started, Self::outcome_for(&e));
                Err(e)
            }
        }
    }

    async fn delete_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
    ) -> Result<bool, FabricPeerError> {
        let started = Instant::now();
        let result = with_retry(|| {
            let mut client = self.client.clone();
            let req = pb::DeleteFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index,
                tenant_id: Some(pb::OrgId {
                    value: tenant_id.0.to_string(),
                }),
            };
            async move { client.delete_fragment(req).await }
        })
        .await
        .map_err(|s| status_to_fabric_err(&s));

        match result {
            Ok(resp) => {
                self.record(op_label::DELETE, started, outcome::OK);
                Ok(resp.into_inner().deleted)
            }
            Err(e) => {
                self.record(op_label::DELETE, started, Self::outcome_for(&e));
                Err(e)
            }
        }
    }

    async fn has_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<bool, FabricPeerError> {
        let started = Instant::now();
        let result = with_retry(|| {
            let mut client = self.client.clone();
            let req = pb::HasFragmentRequest {
                chunk_id: Some(pb::ChunkId {
                    value: chunk_id.0.to_vec(),
                }),
                fragment_index,
            };
            async move { client.has_fragment(req).await }
        })
        .await
        .map_err(|s| status_to_fabric_err(&s));

        match result {
            Ok(resp) => {
                self.record(op_label::HAS, started, outcome::OK);
                Ok(resp.into_inner().present)
            }
            Err(e) => {
                self.record(op_label::HAS, started, Self::outcome_for(&e));
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status code → retriable boolean lookup table. Locks the
    /// retry policy so a future tweak doesn't silently retry a
    /// permanent error like `NotFound` (which would mask real signal
    /// from the `ClusteredChunkStore` read ladder).
    #[test]
    fn retriable_table_matches_policy() {
        assert!(is_retriable_status(&Status::unavailable("network")));
        assert!(is_retriable_status(&Status::deadline_exceeded("slow")));
        assert!(is_retriable_status(&Status::cancelled("client gone")));
        assert!(!is_retriable_status(&Status::not_found("missing")));
        assert!(!is_retriable_status(&Status::permission_denied("san")));
        assert!(!is_retriable_status(&Status::invalid_argument("bad")));
        assert!(!is_retriable_status(&Status::internal("bug")));
        assert!(!is_retriable_status(&Status::data_loss("corrupted")));
    }

    /// Status → `FabricPeerError` mapping.
    #[test]
    fn status_to_fabric_err_maps_known_codes() {
        let e = status_to_fabric_err(&Status::not_found("nope"));
        assert!(matches!(e, FabricPeerError::NotFound));
        let e = status_to_fabric_err(&Status::unavailable("offline"));
        assert!(matches!(e, FabricPeerError::Unavailable(_)));
        let e = status_to_fabric_err(&Status::permission_denied("san"));
        assert!(matches!(e, FabricPeerError::Rejected(_)));
        let e = status_to_fabric_err(&Status::invalid_argument("bad"));
        assert!(matches!(e, FabricPeerError::Transport(_)));
    }

    /// `with_retry` retries exactly once on `Unavailable` and stops.
    #[tokio::test(start_paused = true)]
    async fn with_retry_retries_once_on_unavailable() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(0);
        let result: Result<u32, Status> = with_retry(|| async {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                Err(Status::unavailable("first try"))
            } else {
                Ok(n)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2, "second attempt succeeded");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// `with_retry` does NOT retry on a non-retriable status.
    #[tokio::test(start_paused = true)]
    async fn with_retry_does_not_retry_on_not_found() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(0);
        let result: Result<u32, Status> = with_retry(|| async {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(Status::not_found("never retry"))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "must not retry on NotFound"
        );
    }

    /// `with_retry` gives up after the second failure.
    #[tokio::test(start_paused = true)]
    async fn with_retry_gives_up_after_one_retry() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(0);
        let result: Result<u32, Status> = with_retry(|| async {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(Status::unavailable("forever down"))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "exactly two attempts: original + one retry"
        );
    }

    /// Phase 15c.8 + 15c.10 perf: tonic's default 4 MiB cap is below
    /// the kiseki single-chunk envelope size for typical S3 PUTs.
    /// The fabric must lift the cap or any 4+ MiB write returns
    /// `quorum lost: only 1/2 replicas acked` (the receiver rejects
    /// the `PutFragment` payload as oversized; the sender sees zero
    /// peer acks and only the leader's local write counts).
    ///
    /// Floor: 128 MiB. The gateway stores each S3 PUT as one
    /// envelope; e2e workloads (model weights, training
    /// checkpoints, large dataset shards) routinely PUT 100+ MiB
    /// objects. The e2e witness for the >64 MiB case lives at
    /// `tests/e2e/test_s3_gateway.py::
    /// test_s3_large_put_exceeds_64mib_fabric_cap`.
    #[test]
    fn fabric_max_message_size_accommodates_real_workload_chunks() {
        // const-block assertion evaluates at compile time; this
        // test exists so the contract has a discoverable name +
        // comment, and so a future "let's tighten this" change
        // has to think twice and update the floor in lockstep.
        const _: () = assert!(FABRIC_MAX_MESSAGE_BYTES >= 128 * 1024 * 1024);
    }

    /// The transport cap MUST be strictly larger than the
    /// ciphertext cap. Without headroom the prost wrapper's
    /// field tags + length varints push the encoded request just
    /// over the limit; tonic returns
    /// `Status::resource_exhausted` and the leader's transport
    /// layer surfaces it as "h2 protocol error: http2 error".
    /// Captured live on the 2026-05-04 GCP transport-profile
    /// run — see `.gcp-build/findings/2026-05-04-fabric-256mib-cap/`.
    #[test]
    fn fabric_message_cap_has_strict_headroom_above_ciphertext_cap() {
        const _: () = assert!(FABRIC_MAX_MESSAGE_BYTES > FABRIC_CIPHERTEXT_MAX_BYTES);
        const _: () = assert!(FABRIC_WRAPPER_HEADROOM_BYTES >= 1024);
    }
}
