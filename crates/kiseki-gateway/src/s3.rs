//! S3 gateway (ADR-014).
//!
//! S3 API subset translating object operations to `GatewayOps`.
//! Full implementation requires S3 signature verification and HTTP routing;
//! this provides the domain-level `GetObject`/`PutObject` path.

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;
use crate::ops::{GatewayOps, ReadRequest, WriteRequest};

/// S3 `GetObject` request.
#[derive(Clone, Copy, Debug)]
pub struct GetObjectRequest {
    /// Tenant (derived from S3 bucket or auth).
    pub tenant_id: OrgId,
    /// Namespace (S3 bucket).
    pub namespace_id: NamespaceId,
    /// Object key (mapped to composition ID).
    pub composition_id: CompositionId,
}

/// S3 `GetObject` response.
#[derive(Clone, Debug)]
pub struct GetObjectResponse {
    /// Object body (plaintext).
    pub body: Vec<u8>,
    /// Content length.
    pub content_length: u64,
    /// Content-Type carried through from PUT (RFC 6838).
    pub content_type: Option<String>,
}

/// S3 `PutObject` request.
#[derive(Clone, Debug)]
pub struct PutObjectRequest {
    /// Tenant.
    pub tenant_id: OrgId,
    /// Namespace (S3 bucket).
    pub namespace_id: NamespaceId,
    /// Object body (plaintext).
    pub body: Vec<u8>,
    /// Content-Type to attach for round-trip on GET (RFC 6838).
    pub content_type: Option<String>,
    /// User-supplied object key (URL path component). The S3 PUT
    /// path passes this through to the gateway's `name` field so the
    /// composition is bound in the namespace's secondary index. When
    /// `None`, the object is unnamed (UUID-only) — useful for tests
    /// or programmatic callers that address by `composition_id` only.
    pub key: Option<String>,
    /// Optional HTTP-derived conditional check applied against the
    /// existing key binding. `IfNoneMatch` corresponds to the S3
    /// `x-amz-` and HTTP `If-None-Match: *` semantics; `IfMatch`
    /// corresponds to `If-Match: <etag>`.
    pub conditional: Option<crate::ops::WriteConditional>,
    /// Optional workflow correlation token — the S3 layer parses
    /// `x-kiseki-workflow-ref: <uuid>` from the PUT headers and
    /// passes the 16-byte handle through. The gateway validates it
    /// against its shared `WorkflowTable` and emits per-result
    /// counter ticks; the request itself proceeds either way (I-WA1).
    pub workflow_ref: Option<[u8; 16]>,
}

/// S3 `PutObject` response.
#[derive(Clone, Debug)]
pub struct PutObjectResponse {
    /// `ETag` (composition ID).
    pub etag: String,
}

/// S3 gateway — translates S3 operations to `GatewayOps`.
pub struct S3Gateway<G: GatewayOps> {
    inner: G,
}

impl<G: GatewayOps> S3Gateway<G> {
    /// Create a new S3 gateway wrapping a `GatewayOps` implementation.
    #[must_use]
    pub fn new(inner: G) -> Self {
        Self { inner }
    }

    /// S3 `GetObject` — reads an object and returns the plaintext body
    /// plus stored Content-Type (RFC 6838 round-trip).
    pub async fn get_object(
        &self,
        req: GetObjectRequest,
    ) -> Result<GetObjectResponse, GatewayError> {
        let read_resp = self
            .inner
            .read(ReadRequest {
                tenant_id: req.tenant_id,
                namespace_id: req.namespace_id,
                composition_id: req.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await?;

        Ok(GetObjectResponse {
            content_length: read_resp.data.len() as u64,
            content_type: read_resp.content_type,
            body: read_resp.data,
        })
    }

    /// S3 `PutObject` — writes an object, returns the `ETag`. The
    /// optional Content-Type is attached to the resulting composition
    /// so a subsequent GET on any gateway instance round-trips it
    /// (RFC 6838 / ADV-PA-4).
    pub async fn put_object(
        &self,
        req: PutObjectRequest,
    ) -> Result<PutObjectResponse, GatewayError> {
        let write_resp = self
            .inner
            .write(WriteRequest {
                tenant_id: req.tenant_id,
                namespace_id: req.namespace_id,
                data: req.body,
                name: req.key,
                conditional: req.conditional,
                workflow_ref: req.workflow_ref,
            })
            .await?;

        if req.content_type.is_some() {
            self.inner
                .set_object_content_type(write_resp.composition_id, req.content_type)
                .await?;
        }

        Ok(PutObjectResponse {
            etag: write_resp.composition_id.0.to_string(),
        })
    }

    /// S3 `ListObjectsV2` — lists objects in a bucket by `composition_id`.
    pub async fn list_objects(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        self.inner.list(tenant_id, namespace_id).await
    }

    /// S3 `ListObjectsV2` — lists objects in a bucket by URL key.
    /// Returns only objects with a name binding; complementary to
    /// `list_objects` (which surfaces all compositions). The HTTP
    /// LIST handler merges both for full coverage.
    pub async fn list_named(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId, u64)>, GatewayError> {
        self.inner.list_named(tenant_id, namespace_id, prefix).await
    }

    /// Resolve a URL key to a `composition_id` via the per-bucket name
    /// index. Returns `None` when no composition is bound to the key.
    pub async fn lookup_object_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, GatewayError> {
        self.inner
            .lookup_object_by_name(tenant_id, namespace_id, name)
            .await
    }

    /// Delete an object by URL key (S3 DELETE-by-key). Resolves the
    /// name to a `composition_id` then routes through the standard
    /// delete path so chunk refcounts and the Raft Delete delta are
    /// emitted normally.
    pub async fn delete_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        self.inner
            .delete_by_name(tenant_id, namespace_id, name)
            .await
    }

    /// Ensure a namespace exists for a bucket.
    pub async fn ensure_namespace(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        self.inner.ensure_namespace(tenant_id, namespace_id).await
    }

    /// S3 `DeleteObject` — deletes an object by composition ID.
    pub async fn delete_object(&self, req: DeleteObjectRequest) -> Result<(), GatewayError> {
        self.inner
            .delete(req.tenant_id, req.namespace_id, req.composition_id)
            .await
    }

    /// S3 `HeadObject` — gets object metadata without the body.
    pub async fn head_object(
        &self,
        req: GetObjectRequest,
    ) -> Result<HeadObjectResponse, GatewayError> {
        // Full read to get size (in production, metadata-only path).
        let resp = self
            .inner
            .read(ReadRequest {
                tenant_id: req.tenant_id,
                namespace_id: req.namespace_id,
                composition_id: req.composition_id,
                offset: 0,
                length: u64::MAX,
            })
            .await?;
        Ok(HeadObjectResponse {
            content_length: resp.data.len() as u64,
            etag: req.composition_id.0.to_string(),
        })
    }
}

/// S3 `DeleteObject` request.
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct DeleteObjectRequest {
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
    pub composition_id: CompositionId,
}

/// S3 `HeadObject` response.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct HeadObjectResponse {
    pub content_length: u64,
    pub etag: String,
}

/// S3 `CreateMultipartUpload` request.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct CreateMultipartUploadRequest {
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
}

/// S3 `CreateMultipartUpload` response.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct CreateMultipartUploadResponse {
    pub upload_id: String,
}

/// S3 `UploadPart` request.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct UploadPartRequest {
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
    pub upload_id: String,
    pub part_number: u32,
    pub body: Vec<u8>,
}

/// S3 `UploadPart` response.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct UploadPartResponse {
    pub etag: String,
}

/// S3 `CompleteMultipartUpload` request.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct CompleteMultipartUploadRequest {
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
    pub upload_id: String,
    /// URL key the multipart upload was issued against. When `Some`,
    /// the gateway binds the resulting composition to this name in
    /// the per-bucket index so subsequent GET / DELETE / LIST by
    /// key resolve cleanly. Without this, multipart-uploaded
    /// objects would only be addressable by their UUID — a
    /// regression of the per-key naming work for plain PUT.
    pub key: Option<String>,
}

/// S3 `CompleteMultipartUpload` response.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct CompleteMultipartUploadResponse {
    pub etag: String,
}

/// S3 `AbortMultipartUpload` request.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct AbortMultipartUploadRequest {
    pub upload_id: String,
}

impl<G: GatewayOps> S3Gateway<G> {
    /// S3 `CreateMultipartUpload`.
    pub async fn create_multipart_upload(
        &self,
        req: &CreateMultipartUploadRequest,
    ) -> Result<CreateMultipartUploadResponse, GatewayError> {
        let upload_id = self.inner.start_multipart(req.namespace_id).await?;
        Ok(CreateMultipartUploadResponse { upload_id })
    }

    /// S3 `UploadPart`.
    pub async fn upload_part(
        &self,
        req: &UploadPartRequest,
    ) -> Result<UploadPartResponse, GatewayError> {
        let etag = self
            .inner
            .upload_part(&req.upload_id, req.part_number, &req.body)
            .await?;
        Ok(UploadPartResponse { etag })
    }

    /// S3 `CompleteMultipartUpload`. When the request carries a
    /// `key`, the resulting composition is bound to it in the
    /// gateway's name index so subsequent GET-by-key / DELETE-by-key
    /// / LIST behave the same as for plain PUT — and the binding is
    /// emitted via the Raft Create-delta so followers replay it
    /// (multi-node correctness, vs. the prior local-only bind).
    pub async fn complete_multipart_upload(
        &self,
        req: &CompleteMultipartUploadRequest,
    ) -> Result<CompleteMultipartUploadResponse, GatewayError> {
        let comp_id = self
            .inner
            .complete_multipart(&req.upload_id, req.key.as_deref())
            .await?;
        Ok(CompleteMultipartUploadResponse {
            etag: comp_id.0.to_string(),
        })
    }

    /// S3 `AbortMultipartUpload`.
    pub async fn abort_multipart_upload(
        &self,
        req: &AbortMultipartUploadRequest,
    ) -> Result<(), GatewayError> {
        self.inner.abort_multipart(&req.upload_id).await
    }
}
