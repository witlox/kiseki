//! Peer abstraction for the cluster fabric.
//!
//! The gRPC `ClusterChunkService` client lives behind this trait so
//! [`crate::ClusteredChunkStore`] stays unit-testable with mock peers.
//! The real gRPC implementation lives in [`grpc`].

use async_trait::async_trait;
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_crypto::envelope::Envelope;
use thiserror::Error;

pub mod grpc;

pub use grpc::{
    is_retriable_status, status_to_fabric_err, GrpcFabricPeer, FABRIC_CIPHERTEXT_MAX_BYTES,
    FABRIC_MAX_MESSAGE_BYTES, FABRIC_WRAPPER_HEADROOM_BYTES,
};

/// Errors a fabric peer call can fail with. Maps onto the gRPC
/// status codes in the real impl: `NOT_FOUND` → `NotFound`,
/// `UNAVAILABLE` → `Unavailable`, etc.
#[derive(Clone, Debug, Error)]
pub enum FabricPeerError {
    /// Peer reachable but does not hold the requested fragment.
    #[error("fragment not found")]
    NotFound,
    /// Peer unreachable (network partition, node down).
    #[error("peer unavailable: {0}")]
    Unavailable(String),
    /// Peer rejected the call (auth/SAN failure or bad request).
    #[error("peer rejected: {0}")]
    Rejected(String),
    /// Catch-all for transport / protocol errors.
    #[error("peer transport error: {0}")]
    Transport(String),
}

/// One remote node in the cluster's fabric. The implementation owns
/// a connection (or a connection pool — step 6) to the peer's
/// `ClusterChunkService` endpoint.
#[async_trait]
pub trait FabricPeer: Send + Sync {
    /// Human-readable identifier used in logs / metrics. Typically
    /// the peer's node id.
    fn name(&self) -> &str;

    /// Place a fragment on the peer's local chunk store.
    async fn put_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
        pool_id: String,
        envelope: Envelope,
    ) -> Result<bool, FabricPeerError>;

    /// Read a fragment from the peer's local chunk store.
    async fn get_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<Envelope, FabricPeerError>;

    /// Delete a peer's local fragment. Idempotent: deleting an
    /// absent fragment returns `Ok(false)`.
    async fn delete_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
    ) -> Result<bool, FabricPeerError>;

    /// Probe whether the peer holds a fragment (for repair scrub —
    /// step 16b).
    async fn has_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<bool, FabricPeerError>;
}
