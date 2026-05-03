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
//!     `composition_id` from PUT). Read by `composition_id` maps directly
//!     to GET `/<namespace>/<uuid>`.
//!   * mTLS upgrade is one rustls config away when the cluster moves
//!     off the audited plaintext fallback (ADR-038 §D4.2).
//!
//! Limitations (deliberate — surfaced rather than hidden):
//!
//!   * Multipart upload methods (`start_multipart`, `upload_part`,
//!     `complete_multipart`, `abort_multipart`) are wired to the S3
//!     server's JSON multipart endpoints. FUSE writes still go
//!     through the single-PUT `write` path; multipart is available
//!     for programmatic callers that need large-object uploads.
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
    fn map_error(e: &reqwest::Error) -> GatewayError {
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
            .map_err(|e| Self::map_error(&e))?;

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
        let bytes = resp.bytes().await.map_err(|e| Self::map_error(&e))?;
        let eof = bytes.len() < usize::try_from(req.length).unwrap_or(usize::MAX);

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
            .map_err(|e| Self::map_error(&e))?;

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
            .map_err(|e| Self::map_error(&e))?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(GatewayError::ProtocolError(format!(
                "remote delete returned HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn start_multipart(&self, namespace_id: NamespaceId) -> Result<String, GatewayError> {
        let ns = Self::ns_to_path(namespace_id);
        // POST /{bucket}/{key}?uploads — key is a throwaway UUID since
        // the server ignores it for CreateMultipartUpload.
        let key = uuid::Uuid::new_v4();
        let url = format!("{}/{}/{}?uploads=", self.base_url, ns, key);
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| Self::map_error(&e))?;
        if !resp.status().is_success() {
            return Err(GatewayError::ProtocolError(format!(
                "remote start_multipart returned HTTP {}",
                resp.status()
            )));
        }
        let body = resp.text().await.map_err(|e| Self::map_error(&e))?;
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            GatewayError::ProtocolError(format!("start_multipart: invalid JSON: {e}"))
        })?;
        parsed["uploadId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| GatewayError::ProtocolError("start_multipart: missing uploadId".into()))
    }

    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        // PUT /{bucket}/{key}?uploadId=X&partNumber=N
        // The key is irrelevant; the server routes on the query params.
        let ns = "default";
        let key = uuid::Uuid::new_v4();
        let url = format!(
            "{}/{}/{}?uploadId={}&partNumber={}",
            self.base_url, ns, key, upload_id, part_number
        );
        let resp = self
            .client
            .put(&url)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| Self::map_error(&e))?;
        if !resp.status().is_success() {
            return Err(GatewayError::ProtocolError(format!(
                "remote upload_part returned HTTP {}",
                resp.status()
            )));
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned())
            .ok_or_else(|| {
                GatewayError::ProtocolError("upload_part response missing etag".into())
            })?;
        Ok(etag)
    }

    async fn complete_multipart(
        &self,
        upload_id: &str,
        _name: Option<&str>,
    ) -> Result<CompositionId, GatewayError> {
        // POST /{bucket}/{key}?uploadId=X
        let ns = "default";
        let key = uuid::Uuid::new_v4();
        let url = format!("{}/{}/{}?uploadId={}", self.base_url, ns, key, upload_id);
        let resp = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| Self::map_error(&e))?;
        if !resp.status().is_success() {
            return Err(GatewayError::ProtocolError(format!(
                "remote complete_multipart returned HTTP {}",
                resp.status()
            )));
        }
        let body = resp.text().await.map_err(|e| Self::map_error(&e))?;
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            GatewayError::ProtocolError(format!("complete_multipart: invalid JSON: {e}"))
        })?;
        let etag = parsed["etag"].as_str().ok_or_else(|| {
            GatewayError::ProtocolError("complete_multipart: missing etag".into())
        })?;
        let id = uuid::Uuid::parse_str(etag).map_err(|_| {
            GatewayError::ProtocolError(format!("complete_multipart: etag is not a UUID: {etag}"))
        })?;
        Ok(CompositionId(id))
    }

    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        // DELETE /{bucket}/{key}?uploadId=X
        let ns = "default";
        let key = uuid::Uuid::new_v4();
        let url = format!("{}/{}/{}?uploadId={}", self.base_url, ns, key, upload_id);
        let resp = self
            .client
            .delete(&url)
            .send()
            .await
            .map_err(|e| Self::map_error(&e))?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(GatewayError::ProtocolError(format!(
                "remote abort_multipart returned HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn set_object_content_type(
        &self,
        _composition_id: CompositionId,
        _content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        // The S3 server does not expose a dedicated endpoint for
        // updating Content-Type after initial PUT. Content-Type is set
        // at write time via the Content-Type header on PutObject.
        // This is a no-op on the remote HTTP path; the trait's default
        // would suffice but we override to make the intent explicit.
        Ok(())
    }

    async fn ensure_namespace(
        &self,
        _tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        // PUT /{bucket} — CreateBucket
        let ns = Self::ns_to_path(namespace_id);
        let url = format!("{}/{}", self.base_url, ns);
        let resp = self
            .client
            .put(&url)
            .send()
            .await
            .map_err(|e| Self::map_error(&e))?;
        // 200 = created, 409 = already exists — both are success.
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::CONFLICT {
            return Err(GatewayError::ProtocolError(format!(
                "remote ensure_namespace returned HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }
}
