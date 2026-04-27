//! NFS gateway (ADR-013).
//!
//! NFSv4.1 server translating POSIX operations to `GatewayOps`.
//! Full implementation requires an NFS protocol library; this provides
//! the domain-level read/write path.

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;
use crate::ops::{GatewayOps, ReadRequest, WriteRequest};

/// NFS READ request.
#[derive(Clone, Copy, Debug)]
pub struct NfsReadRequest {
    /// Tenant (from mTLS certificate).
    pub tenant_id: OrgId,
    /// Namespace (NFS export).
    pub namespace_id: NamespaceId,
    /// File handle (mapped to composition ID).
    pub composition_id: CompositionId,
    /// Byte offset.
    pub offset: u64,
    /// Byte count.
    pub count: u32,
}

/// NFS READ response.
#[derive(Clone, Debug)]
pub struct NfsReadResponse {
    /// Data bytes.
    pub data: Vec<u8>,
    /// Whether EOF was reached.
    pub eof: bool,
}

/// NFS WRITE request.
#[derive(Clone, Debug)]
pub struct NfsWriteRequest {
    /// Tenant.
    pub tenant_id: OrgId,
    /// Namespace (NFS export).
    pub namespace_id: NamespaceId,
    /// Data to write.
    pub data: Vec<u8>,
}

/// NFS WRITE response.
#[derive(Clone, Debug)]
pub struct NfsWriteResponse {
    /// Bytes written.
    pub count: u32,
    /// Composition ID of the written file.
    pub composition_id: CompositionId,
}

/// NFS gateway — translates NFS operations to `GatewayOps`.
pub struct NfsGateway<G: GatewayOps> {
    inner: G,
}

impl<G: GatewayOps> NfsGateway<G> {
    /// Create a new NFS gateway wrapping a `GatewayOps` implementation.
    #[must_use]
    pub fn new(inner: G) -> Self {
        Self { inner }
    }

    /// NFS READ — reads from a file handle at a given offset.
    pub async fn read(&self, req: NfsReadRequest) -> Result<NfsReadResponse, GatewayError> {
        let read_resp = self
            .inner
            .read(ReadRequest {
                tenant_id: req.tenant_id,
                namespace_id: req.namespace_id,
                composition_id: req.composition_id,
                offset: req.offset,
                length: u64::from(req.count),
            })
            .await?;

        Ok(NfsReadResponse {
            data: read_resp.data,
            eof: read_resp.eof,
        })
    }

    /// NFS WRITE — writes data to a new file.
    #[allow(clippy::cast_possible_truncation)]
    pub async fn write(&self, req: NfsWriteRequest) -> Result<NfsWriteResponse, GatewayError> {
        let write_resp = self
            .inner
            .write(WriteRequest {
                tenant_id: req.tenant_id,
                namespace_id: req.namespace_id,
                data: req.data,
            })
            .await?;

        Ok(NfsWriteResponse {
            count: write_resp.bytes_written as u32,
            composition_id: write_resp.composition_id,
        })
    }

    /// List compositions in a namespace (Phase 15c.3 — drives the
    /// NFS LOOKUP-by-UUID + READDIR enumeration paths). Returns
    /// `(composition_id, size)` pairs for compositions that belong
    /// to the requested tenant.
    pub async fn list(
        &self,
        tenant_id: kiseki_common::ids::OrgId,
        namespace_id: kiseki_common::ids::NamespaceId,
    ) -> Result<Vec<(kiseki_common::ids::CompositionId, u64)>, GatewayError> {
        self.inner.list(tenant_id, namespace_id).await
    }
}
