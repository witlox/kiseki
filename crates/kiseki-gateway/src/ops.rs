//! Gateway operations trait — protocol-agnostic read/write surface.

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;

/// A read request from a protocol client.
#[derive(Clone, Debug)]
pub struct ReadRequest {
    /// Tenant making the request.
    pub tenant_id: OrgId,
    /// Target namespace.
    pub namespace_id: NamespaceId,
    /// Target composition (file or object).
    pub composition_id: CompositionId,
    /// Byte offset.
    pub offset: u64,
    /// Number of bytes to read.
    pub length: u64,
}

/// A read response.
#[derive(Clone, Debug)]
pub struct ReadResponse {
    /// Plaintext data (decrypted by the gateway for protocol clients).
    pub data: Vec<u8>,
    /// Whether end-of-file was reached.
    pub eof: bool,
}

/// A write request from a protocol client.
#[derive(Clone, Debug)]
pub struct WriteRequest {
    /// Tenant making the request.
    pub tenant_id: OrgId,
    /// Target namespace.
    pub namespace_id: NamespaceId,
    /// Plaintext data (will be encrypted by the gateway, I-K1).
    pub data: Vec<u8>,
}

/// A write response.
#[derive(Clone, Debug)]
pub struct WriteResponse {
    /// Composition ID of the written object.
    pub composition_id: CompositionId,
    /// Number of bytes written.
    pub bytes_written: u64,
}

/// Protocol-agnostic gateway operations.
///
/// All methods take `&self` (not `&mut self`) because implementations
/// use interior mutability — matching the `LogOps` pattern. This allows
/// concurrent readers and writers on a shared gateway instance.
#[async_trait::async_trait]
pub trait GatewayOps: Send + Sync {
    /// Read data from a composition (decrypt + return plaintext to client).
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError>;

    /// Write data to a composition (encrypt plaintext from client → store).
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError>;

    /// List compositions in a namespace. Returns `(composition_id, size)` pairs.
    async fn list(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        // Default: empty list (override in implementations that support it).
        let _ = (tenant_id, namespace_id);
        Ok(Vec::new())
    }

    /// Delete a composition by ID.
    async fn delete(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let _ = (tenant_id, namespace_id, composition_id);
        Err(GatewayError::ProtocolError("delete not supported".into()))
    }

    /// Start a multipart upload. Returns upload ID.
    async fn start_multipart(&self, namespace_id: NamespaceId) -> Result<String, GatewayError> {
        let _ = namespace_id;
        Err(GatewayError::OperationNotSupported(
            "multipart not supported".into(),
        ))
    }

    /// Upload a part of a multipart upload. Returns part `ETag`.
    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        let _ = (upload_id, part_number, data);
        Err(GatewayError::OperationNotSupported(
            "multipart not supported".into(),
        ))
    }

    /// Complete a multipart upload. Returns composition ID.
    async fn complete_multipart(&self, upload_id: &str) -> Result<CompositionId, GatewayError> {
        let _ = upload_id;
        Err(GatewayError::OperationNotSupported(
            "multipart not supported".into(),
        ))
    }

    /// Abort a multipart upload.
    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        let _ = upload_id;
        Err(GatewayError::OperationNotSupported(
            "multipart not supported".into(),
        ))
    }

    /// Ensure a namespace exists in the composition store.
    ///
    /// Called by `create_bucket` to register the namespace before any
    /// object writes target it. Default is a no-op (namespace already exists).
    async fn ensure_namespace(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        let _ = (tenant_id, namespace_id);
        Ok(())
    }
}

/// Blanket impl: `Arc<G>` delegates to `G` via deref.
#[async_trait::async_trait]
impl<G: GatewayOps> GatewayOps for std::sync::Arc<G> {
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        (**self).read(req).await
    }
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        (**self).write(req).await
    }
    async fn list(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<Vec<(CompositionId, u64)>, GatewayError> {
        (**self).list(tenant_id, namespace_id).await
    }
    async fn delete(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        (**self)
            .delete(tenant_id, namespace_id, composition_id)
            .await
    }
    async fn start_multipart(&self, namespace_id: NamespaceId) -> Result<String, GatewayError> {
        (**self).start_multipart(namespace_id).await
    }
    async fn upload_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: &[u8],
    ) -> Result<String, GatewayError> {
        (**self).upload_part(upload_id, part_number, data).await
    }
    async fn complete_multipart(&self, upload_id: &str) -> Result<CompositionId, GatewayError> {
        (**self).complete_multipart(upload_id).await
    }
    async fn abort_multipart(&self, upload_id: &str) -> Result<(), GatewayError> {
        (**self).abort_multipart(upload_id).await
    }
    async fn ensure_namespace(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
    ) -> Result<(), GatewayError> {
        (**self).ensure_namespace(tenant_id, namespace_id).await
    }
}
