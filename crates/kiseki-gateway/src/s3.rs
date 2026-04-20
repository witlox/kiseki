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
}
