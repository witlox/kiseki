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

    /// S3 `GetObject` — reads an object and returns the plaintext body.
    pub fn get_object(&self, req: GetObjectRequest) -> Result<GetObjectResponse, GatewayError> {
        let read_resp = self.inner.read(ReadRequest {
            tenant_id: req.tenant_id,
            namespace_id: req.namespace_id,
            composition_id: req.composition_id,
            offset: 0,
            length: u64::MAX,
        })?;

        Ok(GetObjectResponse {
            content_length: read_resp.data.len() as u64,
            body: read_resp.data,
        })
    }

    /// S3 `PutObject` — writes an object, returns the `ETag`.
    pub fn put_object(&self, req: PutObjectRequest) -> Result<PutObjectResponse, GatewayError> {
        let write_resp = self.inner.write(WriteRequest {
            tenant_id: req.tenant_id,
            namespace_id: req.namespace_id,
            data: req.body,
        })?;

        Ok(PutObjectResponse {
            etag: write_resp.composition_id.0.to_string(),
        })
    }

    /// S3 `ListObjectsV2` — lists objects in a bucket.
    pub fn list_objects(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        self.inner.list(tenant_id, namespace_id)
    }

    /// S3 `DeleteObject` — deletes an object by composition ID.
    pub fn delete_object(&self, req: DeleteObjectRequest) -> Result<(), GatewayError> {
        self.inner
            .delete(req.tenant_id, req.namespace_id, req.composition_id)
    }

    /// S3 `HeadObject` — gets object metadata without the body.
    pub fn head_object(&self, req: GetObjectRequest) -> Result<HeadObjectResponse, GatewayError> {
        // Full read to get size (in production, metadata-only path).
        let resp = self.inner.read(ReadRequest {
            tenant_id: req.tenant_id,
            namespace_id: req.namespace_id,
            composition_id: req.composition_id,
            offset: 0,
            length: u64::MAX,
        })?;
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
    pub fn create_multipart_upload(
        &self,
        req: &CreateMultipartUploadRequest,
    ) -> Result<CreateMultipartUploadResponse, GatewayError> {
        let upload_id = self.inner.start_multipart(req.namespace_id)?;
        Ok(CreateMultipartUploadResponse { upload_id })
    }

    /// S3 `UploadPart`.
    pub fn upload_part(&self, req: &UploadPartRequest) -> Result<UploadPartResponse, GatewayError> {
        let etag = self
            .inner
            .upload_part(&req.upload_id, req.part_number, &req.body)?;
        Ok(UploadPartResponse { etag })
    }

    /// S3 `CompleteMultipartUpload`.
    pub fn complete_multipart_upload(
        &self,
        req: &CompleteMultipartUploadRequest,
    ) -> Result<CompleteMultipartUploadResponse, GatewayError> {
        let comp_id = self.inner.complete_multipart(&req.upload_id)?;
        Ok(CompleteMultipartUploadResponse {
            etag: comp_id.0.to_string(),
        })
    }

    /// S3 `AbortMultipartUpload`.
    pub fn abort_multipart_upload(
        &self,
        req: &AbortMultipartUploadRequest,
    ) -> Result<(), GatewayError> {
        self.inner.abort_multipart(&req.upload_id)
    }
}
