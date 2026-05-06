//! gRPC binding adapter — `tonic::GatewayDataService` over
//! [`ServerImpl`] (binding-agnostic handler in
//! [`crate::native::server`]).
//!
//! ADR-042 §1.8 enforcement (literal): the
//! `kiseki-gateway::native::server` module never references
//! `tonic::Request` / `tonic::Streaming` / `tonic::Response`. The gRPC
//! binding's wire shape lives entirely here.
//!
//! Per-method shape:
//!
//! 1. Build a `&dyn RequestPrincipal` via
//!    [`super::principal::principal_from_request`].
//! 2. Decode the body via `request.into_inner()`.
//! 3. Call the inherent handler on `Arc<ServerImpl>`.
//! 4. Wrap the response in `tonic::Response::new`.
//!
//! Streaming methods (`put_object_stream`, `get_object_stream`,
//! `put_part`, `read_stream`, `write_stream`) buffer/frame on the
//! `tonic::Streaming` type and dispatch into the unary inherent
//! methods. POSIX-only stubs return `Status::unimplemented` directly
//! without ever crossing the handler boundary.

use std::pin::Pin;
use std::sync::Arc;

use kiseki_proto::v1::native::{
    self as np, gateway_data_service_server::GatewayDataService,
};
use tonic::{Request, Response, Status, Streaming};

use crate::native::server::ServerImpl;

use super::principal::principal_from_request;

/// gRPC binding adapter. Wraps an `Arc<ServerImpl>` and implements
/// the tonic-generated `GatewayDataService`.
#[derive(Clone)]
pub struct GrpcAdapter {
    inner: Arc<ServerImpl>,
}

impl GrpcAdapter {
    /// Build a new gRPC adapter wrapping the binding-agnostic
    /// handler. `Arc<ServerImpl>` is cheap to clone; the same handler
    /// instance can back multiple bindings (gRPC + TCP-framed +
    /// ibverbs concurrently) on the same node.
    #[must_use]
    pub fn new(inner: Arc<ServerImpl>) -> Self {
        Self { inner }
    }

    /// Borrow the underlying handler. Tests that need to manipulate
    /// `ServerImpl`-side state (topology snapshot, lease store) reach
    /// in here.
    #[must_use]
    pub fn inner(&self) -> &Arc<ServerImpl> {
        &self.inner
    }
}

#[tonic::async_trait]
impl GatewayDataService for GrpcAdapter {
    // ----- Object verbs -----

    async fn put_object(
        &self,
        request: Request<np::PutObjectRequest>,
    ) -> Result<Response<np::PutObjectResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner.put_object(&principal, req).await.map(Response::new)
    }

    async fn put_object_stream(
        &self,
        request: Request<Streaming<np::PutObjectChunk>>,
    ) -> Result<Response<np::PutObjectResponse>, Status> {
        // Buffer the stream, then call put_object once. Phase 5+ may
        // optimize for multi-chunk PUT atomicity (F-NG12) — needs an
        // atomic CommitStream barrier with orphan-fragment scrub on
        // partial failure. v1 buffers.
        let principal = principal_from_request(&request);
        let mut stream = request.into_inner();
        let mut header: Option<np::PutObjectRequest> = None;
        let mut data: Vec<u8> = Vec::new();
        let mut committed = false;
        while let Some(chunk) = stream.message().await? {
            let Some(kind) = chunk.kind else {
                return Err(Status::invalid_argument("PutObjectChunk.kind required"));
            };
            match kind {
                np::put_object_chunk::Kind::First(h) => {
                    if header.is_some() {
                        return Err(Status::invalid_argument(
                            "First sent more than once on PutObjectStream",
                        ));
                    }
                    header = Some(h);
                }
                np::put_object_chunk::Kind::Data(b) => {
                    if header.is_none() {
                        return Err(Status::invalid_argument(
                            "Data before First on PutObjectStream",
                        ));
                    }
                    data.extend_from_slice(&b);
                }
                np::put_object_chunk::Kind::Commit(_) => {
                    committed = true;
                    break;
                }
            }
        }
        if !committed {
            return Err(Status::aborted(
                "PutObjectStream ended without explicit Commit",
            ));
        }
        let mut req = header.ok_or_else(|| {
            Status::invalid_argument("PutObjectStream missing First")
        })?;
        req.data = data;
        self.inner
            .put_object(&principal, req)
            .await
            .map(Response::new)
    }

    async fn get_object(
        &self,
        request: Request<np::GetObjectRequest>,
    ) -> Result<Response<np::GetObjectResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner.get_object(&principal, req).await.map(Response::new)
    }

    type GetObjectStreamStream = Pin<
        Box<dyn tokio_stream::Stream<Item = Result<np::GetObjectChunk, Status>> + Send + 'static>,
    >;

    async fn get_object_stream(
        &self,
        request: Request<np::GetObjectRequest>,
    ) -> Result<Response<Self::GetObjectStreamStream>, Status> {
        // v1: identical wire content, just framed. Future work: emit
        // per-chunk sealed envelopes for TrustedCompute and lazy-stream
        // chunks from the chunk store directly.
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        let resp = self.inner.get_object(&principal, req).await?;
        let header = np::GetObjectChunk {
            kind: Some(np::get_object_chunk::Kind::Header(np::GetObjectHeader {
                size: resp.size,
                content_type: resp.content_type,
                etag: resp.etag,
                client_decrypt: false,
            })),
        };
        let data_chunk = np::GetObjectChunk {
            kind: Some(np::get_object_chunk::Kind::Data(resp.data)),
        };
        let s = tokio_stream::iter(vec![Ok(header), Ok(data_chunk)]);
        Ok(Response::new(Box::pin(s)))
    }

    async fn delete_object(
        &self,
        request: Request<np::DeleteObjectRequest>,
    ) -> Result<Response<np::DeleteObjectResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .delete_object(&principal, req)
            .await
            .map(Response::new)
    }

    async fn head_object(
        &self,
        request: Request<np::HeadObjectRequest>,
    ) -> Result<Response<np::HeadObjectResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .head_object(&principal, req)
            .await
            .map(Response::new)
    }

    async fn list_objects(
        &self,
        request: Request<np::ListObjectsRequest>,
    ) -> Result<Response<np::ListObjectsResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .list_objects(&principal, req)
            .await
            .map(Response::new)
    }

    async fn lookup_by_name(
        &self,
        request: Request<np::LookupByNameRequest>,
    ) -> Result<Response<np::LookupByNameResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .lookup_by_name(&principal, req)
            .await
            .map(Response::new)
    }

    // ----- Multipart -----

    async fn init_multipart(
        &self,
        request: Request<np::InitMultipartRequest>,
    ) -> Result<Response<np::InitMultipartResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .init_multipart(&principal, req)
            .await
            .map(Response::new)
    }

    async fn put_part(
        &self,
        request: Request<Streaming<np::PutPartChunk>>,
    ) -> Result<Response<np::PutPartResponse>, Status> {
        let principal = principal_from_request(&request);
        let mut stream = request.into_inner();
        let mut header: Option<np::PutPartHeader> = None;
        let mut data: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.message().await? {
            let Some(kind) = chunk.kind else {
                return Err(Status::invalid_argument("PutPartChunk.kind required"));
            };
            match kind {
                np::put_part_chunk::Kind::Header(h) => {
                    if header.is_some() {
                        return Err(Status::invalid_argument(
                            "Header sent more than once on PutPart",
                        ));
                    }
                    header = Some(h);
                }
                np::put_part_chunk::Kind::Data(b) => {
                    if header.is_none() {
                        return Err(Status::invalid_argument("Data before Header"));
                    }
                    data.extend_from_slice(&b);
                }
            }
        }
        let h = header.ok_or_else(|| Status::invalid_argument("PutPart missing Header"))?;
        self.inner
            .put_part_buffered(&principal, h, data)
            .await
            .map(Response::new)
    }

    async fn complete_multipart(
        &self,
        request: Request<np::CompleteMultipartRequest>,
    ) -> Result<Response<np::CompleteMultipartResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .complete_multipart(&principal, req)
            .await
            .map(Response::new)
    }

    async fn abort_multipart(
        &self,
        request: Request<np::AbortMultipartRequest>,
    ) -> Result<Response<np::AbortMultipartResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .abort_multipart(&principal, req)
            .await
            .map(Response::new)
    }

    // ----- POSIX verbs (stubs returning Status::unimplemented; bridging
    // to a real inode store is a follow-up — see ADR-042 §9 + the
    // implementation plan's open items.) -----

    async fn path_lookup(
        &self,
        _request: Request<np::PathLookupRequest>,
    ) -> Result<Response<np::PathLookupResponse>, Status> {
        Err(Status::unimplemented("POSIX path_lookup not bridged in Phase 2"))
    }

    async fn open(
        &self,
        _request: Request<np::OpenRequest>,
    ) -> Result<Response<np::OpenResponse>, Status> {
        Err(Status::unimplemented("POSIX open not bridged in Phase 2"))
    }

    async fn read(
        &self,
        _request: Request<np::ReadRequest>,
    ) -> Result<Response<np::ReadResponse>, Status> {
        Err(Status::unimplemented("POSIX read not bridged in Phase 2"))
    }

    type ReadStreamStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<np::ReadChunk, Status>> + Send + 'static>>;

    async fn read_stream(
        &self,
        _request: Request<np::ReadRequest>,
    ) -> Result<Response<Self::ReadStreamStream>, Status> {
        Err(Status::unimplemented("POSIX read_stream not bridged in Phase 2"))
    }

    async fn write(
        &self,
        _request: Request<np::WriteRequest>,
    ) -> Result<Response<np::WriteResponse>, Status> {
        Err(Status::unimplemented("POSIX write not bridged in Phase 2"))
    }

    async fn write_stream(
        &self,
        _request: Request<Streaming<np::WriteChunk>>,
    ) -> Result<Response<np::WriteResponse>, Status> {
        Err(Status::unimplemented("POSIX write_stream not bridged in Phase 2"))
    }

    async fn fsync(
        &self,
        _request: Request<np::FsyncRequest>,
    ) -> Result<Response<np::FsyncResponse>, Status> {
        // Cluster-wide flush — no principal needed today; per-handle
        // Fsync lands with POSIX bridging.
        self.inner.fsync().await.map(Response::new)
    }

    async fn close(
        &self,
        _request: Request<np::CloseRequest>,
    ) -> Result<Response<np::CloseResponse>, Status> {
        Err(Status::unimplemented("POSIX close not bridged in Phase 2"))
    }

    async fn setattr(
        &self,
        _request: Request<np::SetattrRequest>,
    ) -> Result<Response<np::SetattrResponse>, Status> {
        Err(Status::unimplemented("POSIX setattr not bridged in Phase 2"))
    }

    async fn getattr(
        &self,
        _request: Request<np::GetattrRequest>,
    ) -> Result<Response<np::GetattrResponse>, Status> {
        Err(Status::unimplemented("POSIX getattr not bridged in Phase 2"))
    }

    type ReadDirStream = Pin<
        Box<dyn tokio_stream::Stream<Item = Result<np::ReadDirEntry, Status>> + Send + 'static>,
    >;

    async fn read_dir(
        &self,
        _request: Request<np::ReadDirRequest>,
    ) -> Result<Response<Self::ReadDirStream>, Status> {
        Err(Status::unimplemented("POSIX read_dir not bridged in Phase 2"))
    }

    async fn mkdir(
        &self,
        _request: Request<np::MkdirRequest>,
    ) -> Result<Response<np::MkdirResponse>, Status> {
        Err(Status::unimplemented("POSIX mkdir not bridged in Phase 2"))
    }

    async fn unlink(
        &self,
        _request: Request<np::UnlinkRequest>,
    ) -> Result<Response<np::UnlinkResponse>, Status> {
        Err(Status::unimplemented("POSIX unlink not bridged in Phase 2"))
    }

    async fn rename_within_shard(
        &self,
        _request: Request<np::RenameRequest>,
    ) -> Result<Response<np::RenameResponse>, Status> {
        Err(Status::unimplemented("POSIX rename not bridged in Phase 2"))
    }

    // ----- Lease verbs -----

    async fn acquire_lease(
        &self,
        request: Request<np::AcquireLeaseRequest>,
    ) -> Result<Response<np::AcquireLeaseResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .acquire_lease(&principal, req)
            .await
            .map(Response::new)
    }

    async fn renew_lease(
        &self,
        request: Request<np::RenewLeaseRequest>,
    ) -> Result<Response<np::RenewLeaseResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .renew_lease(&principal, req)
            .await
            .map(Response::new)
    }

    async fn release_lease(
        &self,
        request: Request<np::ReleaseLeaseRequest>,
    ) -> Result<Response<np::ReleaseLeaseResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .release_lease(&principal, req)
            .await
            .map(Response::new)
    }

    // ----- DEK fetch -----

    async fn fetch_dek(
        &self,
        request: Request<np::FetchDekRequest>,
    ) -> Result<Response<np::FetchDekResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .fetch_dek(&principal, req)
            .await
            .map(Response::new)
    }

    async fn batch_fetch_dek(
        &self,
        request: Request<np::BatchFetchDekRequest>,
    ) -> Result<Response<np::BatchFetchDekResponse>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .batch_fetch_dek(&principal, req)
            .await
            .map(Response::new)
    }

    // ----- Topology -----

    async fn get_topology(
        &self,
        request: Request<np::GetTopologyRequest>,
    ) -> Result<Response<np::TopologyInfo>, Status> {
        let principal = principal_from_request(&request);
        let req = request.into_inner();
        self.inner
            .get_topology(&principal, req)
            .await
            .map(Response::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::native::signing_keys::SigningKeys;
    use kiseki_chunk::ChunkStore;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    fn make_adapter() -> GrpcAdapter {
        let gw = Arc::new(InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        ));
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        let inner = Arc::new(ServerImpl::new(
            gw as Arc<dyn crate::ops::GatewayOps>,
            signing,
        ));
        GrpcAdapter::new(inner)
    }

    /// POSIX verbs are not bridged in this phase — the adapter must
    /// surface `Unimplemented` directly without ever crossing into
    /// `ServerImpl`. Exercise one representative verb (`open`) to pin
    /// the contract.
    #[tokio::test]
    async fn posix_open_returns_unimplemented() {
        let adapter = make_adapter();
        let err = adapter
            .open(Request::new(np::OpenRequest::default()))
            .await
            .expect_err("POSIX open is not bridged in this phase");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    /// `Fsync` is the one POSIX verb the adapter does dispatch into
    /// the handler (cluster-wide flush trigger). Confirm it reaches
    /// `ServerImpl::fsync` and returns Ok with the placeholder
    /// response shape.
    #[tokio::test]
    async fn fsync_dispatches_to_handler() {
        let adapter = make_adapter();
        let resp = adapter
            .fsync(Request::new(np::FsyncRequest::default()))
            .await
            .expect("fsync_pending stub returns Ok")
            .into_inner();
        assert_eq!(resp.fsynced_lsn, 0);
        assert!(resp.shard_id.is_none());
    }
}
