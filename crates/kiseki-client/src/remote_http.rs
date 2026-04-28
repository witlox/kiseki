//! Remote HTTP gateway — bridges `kiseki-client mount` (FUSE) to a
//! running kiseki-server cluster.
//!
//! The FUSE binary historically wired an in-process `InMemoryGateway`,
//! which means a `kiseki-client mount` was a self-contained sandbox —
//! reads/writes never touched the cluster, and the `--endpoint` flag
//! was decorative. Phase 15c.6 closes the gap with a network-attached
//! `GatewayOps` impl over the cluster's S3 listener (port 9000).
//!
//! Why HTTP-over-S3 and not gRPC?
//!
//!   * The cluster already exposes the data plane via HTTP/S3 (port
//!     9000) — same code path as `aws s3 cp`. No new gRPC service or
//!     proto definitions to design.
//!   * S3 keys ARE composition UUIDs (kiseki returns the etag = the
//!     composition_id from PUT). Read by composition_id maps directly
//!     to GET `/<namespace>/<uuid>`.
//!   * mTLS upgrade is one rustls config away when the cluster moves
//!     off the audited plaintext fallback (ADR-038 §D4.2).
//!
//! Limitations (deliberate — surfaced rather than hidden):
//!
//!   * Multipart upload methods stub to `OperationNotSupported` —
//!     FUSE writes through `fuse_daemon::write` go via the single-PUT
//!     `write` path. Multipart support would need the S3 multipart
//!     XML API on the gateway side (kiseki-gateway has it; this
//!     client doesn't surface it for FUSE workloads yet).
//!   * `delete` works (DELETE /<namespace>/<uuid>); `unlink`
//!     bridges to it in `KisekiFuse::unlink_in` so a FUSE rm(1)
//!     deletes the cluster-side composition (Phase 15c.7 closed).
//!   * Encryption keys live SERVER-SIDE: the gateway encrypts with
//!     the namespace's key before chunk store writes, so the FUSE
//!     client sends plaintext. If the deployment requires E2EE the
//!     client must encrypt before PUT — explicit Phase 16 deferral.

#![cfg(feature = "remote-http")]

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

/// Network-attached `GatewayOps` impl. Talks to a kiseki-server's S3
/// listener over plaintext HTTP today; pluggable to HTTPS via the
/// rustls feature on `reqwest` when the cluster is mTLS-only.
pub struct RemoteHttpGateway {
    base_url: String,
    client: reqwest::Client,
}

impl RemoteHttpGateway {
    /// Create a gateway pointing at `http://<host>:<port>` (no
    /// trailing slash). The first segment of every request path is
    /// the namespace name (`default` for the bootstrap namespace).
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let trimmed = base_url.trim_end_matches('/').to_owned();
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest::Client::builder cannot fail with default config");
        Self {
            base_url: trimmed,
            client,
        }
    }

    fn ns_to_path(namespace_id: NamespaceId) -> &'static str {
        // Kiseki's S3 listener exports a single namespace named
        // `default` for the bootstrap tenant. Until per-namespace
        // S3 path mapping lands (Phase 15c.7), every remote-http
        // mount targets `default`.
        let _ = namespace_id;
        "default"
    }

    /// Map a `reqwest::Error` to `GatewayError`. Network errors are
    /// `ProtocolError`; HTTP status codes are recognized as the
    /// closest semantic equivalent.
    fn map_error(e: reqwest::Error) -> GatewayError {
        GatewayError::ProtocolError(format!("remote http: {e}"))
    }
}

#[async_trait::async_trait]
impl GatewayOps for RemoteHttpGateway {
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        let ns = Self::ns_to_path(req.namespace_id);
        let url = format!("{}/{}/{}", self.base_url, ns, req.composition_id.0);

        // Use HTTP Range to fetch [offset, offset+length).
        // Kiseki's S3 server honors RFC 9110 §14.2 byte-range
        // requests; the response is `206 Partial Content` with the
        // requested slice + `Content-Range` for verification.
        let end = req.offset.saturating_add(req.length).saturating_sub(1);
        let range = format!("bytes={}-{}", req.offset, end);

        let resp = self
            .client
            .get(&url)
            .header("Range", &range)
            .send()
            .await
            .map_err(Self::map_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::ProtocolError("composition not found".into()));
        }
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(GatewayError::ProtocolError(format!(
                "remote read returned HTTP {status}"
            )));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // EOF inference: when the server returned the full remaining
        // tail (or 200 OK with a body shorter than requested), mark
        // EOF. The kernel's stat-based EOF check covers most paths
        // but FUSE callers also trust the gateway's eof flag.
        let bytes = resp.bytes().await.map_err(Self::map_error)?;
        let eof = bytes.len() < req.length as usize;

        Ok(ReadResponse {
            data: bytes.to_vec(),
            eof,
            content_type,
        })
    }

    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let ns = Self::ns_to_path(req.namespace_id);

        // Kiseki's S3 PUT auto-generates the composition_id and
        // returns it in the ETag header. The key in the URL is a
        // user-chosen name; for a FUSE-write of a freshly-created
        // file we use a UUID as the name so the same value lands
        // both as the key AND (effectively) as the etag.
        let key = uuid::Uuid::new_v4().to_string();
        let url = format!("{}/{}/{}", self.base_url, ns, key);

        let resp = self
            .client
            .put(&url)
            .body(req.data.clone())
            .send()
            .await
            .map_err(Self::map_error)?;

        let status = resp.status();
        if !status.is_success() {
            return Err(GatewayError::ProtocolError(format!(
                "remote write returned HTTP {status}"
            )));
        }

        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned())
            .ok_or_else(|| GatewayError::ProtocolError("PUT response missing etag".into()))?;

        let composition_id = uuid::Uuid::parse_str(&etag)
            .map(CompositionId)
            .map_err(|_| GatewayError::ProtocolError(format!("etag is not a UUID: {etag}")))?;

        Ok(WriteResponse {
            composition_id,
            bytes_written: req.data.len() as u64,
        })
    }

    async fn list(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        // Kiseki's S3 LIST uses the standard `?list-type=2` query
        // and returns XML. Decoding it correctly is multipart work;
        // for FUSE-`readdir` purposes we don't currently need a
        // remote list (KisekiFuse maintains a local inode table).
        // Returning an empty list is safe — the FUSE adapter falls
        // back to its in-memory directory tree.
        let _ = (tenant_id, namespace_id);
        Ok(Vec::new())
    }

    async fn delete(
        &self,
        _tenant_id: OrgId,
        namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let ns = Self::ns_to_path(namespace_id);
        let url = format!("{}/{}/{}", self.base_url, ns, composition_id.0);
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(Self::map_error)?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::ProtocolError(format!(
                "remote delete returned HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }
}
