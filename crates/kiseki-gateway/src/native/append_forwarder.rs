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

use kiseki_common::ids::{NodeId, SequenceNumber};
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
}
