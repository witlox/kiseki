//! #111: forward a built `ChunkAndDelta` append to the shard leader.
//!
//! When the local node is a follower for a write's target shard, the
//! gateway's forward-aware emit hands the built append here. We re-issue
//! it to the leader's `LogService.AppendChunkAndDelta` over the
//! proxy-client's channel pool (reusing #103's per-node data-port map +
//! the cluster's TLS posture). The leader `client_write`s it locally and
//! replicates back to all shard members — so write / delete /
//! multipart-complete all commit on a remote-led shard, for every
//! ingress (S3 / NFS / FUSE / native), not just the native proxy path.

use std::sync::Arc;

use kiseki_common::ids::{ChunkId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_log::error::LogError;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendForwarder};
use kiseki_proto::v1::log_service_client::LogServiceClient;

use super::proxy_client::ProxyClient;

/// `AppendForwarder` backed by the ADR-042 §4 [`ProxyClient`] channel
/// pool. Dials the shard leader's `LogService` on the same data port the
/// proxy fallback uses.
pub struct ProxyAppendForwarder {
    proxy: Arc<ProxyClient>,
}

impl ProxyAppendForwarder {
    /// Wrap a configured proxy client (its peer-address map + channel
    /// pool are reused for `LogService` calls).
    #[must_use]
    pub fn new(proxy: Arc<ProxyClient>) -> Self {
        Self { proxy }
    }
}

#[async_trait::async_trait]
impl AppendForwarder for ProxyAppendForwarder {
    async fn forward_append(
        &self,
        leader_node: NodeId,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let channel = self.proxy.acquire_channel(leader_node).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                leader = leader_node.0,
                "append-forward (#111): could not reach shard leader",
            );
            LogError::Unavailable
        })?;
        let proto = kiseki_log::grpc::append_chunk_and_delta_request_to_proto(&req);
        let resp = LogServiceClient::new(channel)
            .append_chunk_and_delta(proto)
            .await
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    leader = leader_node.0,
                    "append-forward (#111): leader LogService.AppendChunkAndDelta failed",
                );
                LogError::Unavailable
            })?;
        Ok(SequenceNumber(resp.into_inner().sequence))
    }

    // Phase 16c / I-C2 (#111, delete half): re-issue the cluster
    // refcount decrement against the shard leader's `LogService` over
    // the same channel pool. Loop-safe — the leader commits locally
    // (its handler does not chain another forward).
    async fn forward_decrement_chunk_refcount(
        &self,
        leader_node: NodeId,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<bool, LogError> {
        let channel = self.proxy.acquire_channel(leader_node).await.map_err(|e| {
            tracing::warn!(
                error = %e,
                leader = leader_node.0,
                "decrement-forward (#111): could not reach shard leader",
            );
            LogError::Unavailable
        })?;
        let proto = kiseki_proto::v1::DecrementChunkRefcountRequest {
            shard_id: Some(kiseki_proto::v1::ShardId {
                value: shard_id.0.to_string(),
            }),
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: tenant_id.0.to_string(),
            }),
            chunk_id: Some(kiseki_proto::v1::ChunkId {
                value: chunk_id.0.to_vec(),
            }),
        };
        let resp = LogServiceClient::new(channel)
            .decrement_chunk_refcount(proto)
            .await
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    leader = leader_node.0,
                    "decrement-forward (#111): leader LogService.DecrementChunkRefcount failed",
                );
                LogError::Unavailable
            })?;
        Ok(resp.into_inner().tombstoned)
    }
}
