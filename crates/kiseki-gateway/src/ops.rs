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
#[derive(Clone, Debug, Default)]
pub struct ReadResponse {
    /// Plaintext data (decrypted by the gateway for protocol clients).
    pub data: Vec<u8>,
    /// Whether end-of-file was reached.
    pub eof: bool,
    /// Object Content-Type carried through from PUT (RFC 6838).
    /// Populated from the composition's `content_type` field; `None`
    /// for compositions written without one (e.g. NFS data path).
    pub content_type: Option<String>,
}

/// A write request from a protocol client.
#[derive(Clone, Debug, Default)]
pub struct WriteRequest {
    /// Tenant making the request.
    pub tenant_id: OrgId,
    /// Target namespace.
    pub namespace_id: NamespaceId,
    /// Plaintext data (will be encrypted by the gateway, I-K1).
    pub data: Vec<u8>,
    /// Optional user-supplied key (S3 PUT URL key). When `Some`, the
    /// resulting composition is bound to this name in the namespace's
    /// secondary index so subsequent GET/DELETE/LIST by key works
    /// uniformly across nodes (followers replay the binding via the
    /// hydrator). `None` for paths that don't have a meaningful name
    /// (NFS data path — the file handle is the addressing token).
    pub name: Option<String>,
    /// Optional HTTP-level conditional that gates the write. Evaluated
    /// before any chunk/Raft work — failures return `PreconditionFailed`
    /// without leaving partial state behind.
    pub conditional: Option<WriteConditional>,
    /// Optional workflow correlation token (`x-kiseki-workflow-ref`
    /// header). Validated against the gateway's shared workflow table
    /// (a clone of the advisory subsystem's table). Per I-WA1 the
    /// header is advisory: an unknown ref or a mismatched tenant
    /// **never** blocks the write — it is simply recorded as
    /// `invalid` in the workflow_ref counter and the write proceeds.
    pub workflow_ref: Option<[u8; 16]>,
}

/// HTTP-derived conditional check applied to a `WriteRequest` against
/// the existing name binding (if any). Modeled on RFC 9110 §13.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteConditional {
    /// `If-None-Match: *` — succeed only if no composition is bound to
    /// the given name. S3 conditional-create / `x-amz-copy-source-if-
    /// none-match: *` semantics.
    IfNoneMatch,
    /// `If-Match: <etag>` — succeed only if the named composition
    /// exists and currently maps to the given composition_id.
    IfMatch(CompositionId),
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

    /// Attach a Content-Type to a composition (RFC 6838 round-trip via
    /// composition metadata; survives across gateway instances). Default
    /// no-op for backends that don't track per-object metadata; the
    /// in-memory and persistent backends should override.
    async fn set_object_content_type(
        &self,
        composition_id: CompositionId,
        content_type: Option<String>,
    ) -> Result<(), GatewayError> {
        let _ = (composition_id, content_type);
        Ok(())
    }

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

    /// Resolve `(namespace_id, name)` → composition_id via the per-
    /// bucket secondary index. Returns `None` if no composition is
    /// bound to that name. Used by the S3 GET/HEAD path to map URL
    /// `key` to a real composition.
    ///
    /// Default: returns `None` (backends without name index).
    async fn lookup_object_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, GatewayError> {
        let _ = (tenant_id, namespace_id, name);
        Ok(None)
    }

    /// Bind `name` to an existing composition in the per-bucket name
    /// index. Used by `CompleteMultipartUpload` so a multipart-
    /// uploaded object is addressable by its URL key just like a
    /// plain `PutObject`. Overwrites any existing binding for the
    /// same name (the caller is responsible for conditional checks).
    ///
    /// Default: `Err(NotSupported)`.
    async fn bind_object_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let _ = (tenant_id, namespace_id, name, composition_id);
        Err(GatewayError::OperationNotSupported(
            "bind_object_name not supported".into(),
        ))
    }

    /// Delete a composition by name. Returns `true` if a binding
    /// existed (and was removed); `false` if the name wasn't bound.
    /// The underlying composition is also removed (chunk refcounts
    /// decremented per the standard `delete` path).
    ///
    /// Default: `Err(NotSupported)`.
    async fn delete_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        let _ = (tenant_id, namespace_id, name);
        Err(GatewayError::OperationNotSupported(
            "delete_by_name not supported".into(),
        ))
    }

    /// Enumerate `(name, composition_id, size)` for objects in a
    /// namespace, optionally filtered by `prefix`. S3 LIST returns
    /// these alphabetically by name.
    ///
    /// Default: empty list.
    async fn list_named(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId, u64)>, GatewayError> {
        let _ = (tenant_id, namespace_id, prefix);
        Ok(Vec::new())
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
    ///
    /// `name` is the optional S3 URL key. When `Some`, the resulting
    /// composition is bound to it in the per-bucket name index AND
    /// the binding is emitted via the Raft Create-delta's v2 payload
    /// so followers' hydrators install the same binding. Without
    /// this, multipart-uploaded objects would be GET-by-key only on
    /// the leader (silent 404 on followers).
    async fn complete_multipart(
        &self,
        upload_id: &str,
        name: Option<&str>,
    ) -> Result<CompositionId, GatewayError> {
        let _ = (upload_id, name);
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
    async fn complete_multipart(
        &self,
        upload_id: &str,
        name: Option<&str>,
    ) -> Result<CompositionId, GatewayError> {
        (**self).complete_multipart(upload_id, name).await
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
    async fn lookup_object_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<Option<CompositionId>, GatewayError> {
        (**self)
            .lookup_object_by_name(tenant_id, namespace_id, name)
            .await
    }
    async fn bind_object_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        (**self)
            .bind_object_name(tenant_id, namespace_id, name, composition_id)
            .await
    }
    async fn delete_by_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        (**self)
            .delete_by_name(tenant_id, namespace_id, name)
            .await
    }
    async fn list_named(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, CompositionId, u64)>, GatewayError> {
        (**self).list_named(tenant_id, namespace_id, prefix).await
    }
}
