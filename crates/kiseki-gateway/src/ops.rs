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
    /// `invalid` in the `workflow_ref` counter and the write proceeds.
    pub workflow_ref: Option<[u8; 16]>,
    /// Optional idempotency key (I-NG5 / ADR-042 §6). 1..=64 bytes,
    /// opaque, client-generated. The server-side proxy fallback
    /// (ADR-042 §4) MUST preserve this byte-for-byte when re-issuing
    /// a write against the shard leader so the leader's dedup table
    /// short-circuits a retry to the original response (exactly-once
    /// semantics). Validated at the proto boundary in
    /// `kiseki-gateway::native::server::validate_idempotency_key`;
    /// the field carried here is the post-validation bytes.
    ///
    /// `None` for callers that don't have an idempotency context
    /// (in-process tests, NFS data path — the file handle is the
    /// addressing token and the gateway's per-handle bookkeeping
    /// covers retries).
    pub idempotency_key: Option<Vec<u8>>,
    /// Optional forwarding attribution (audit I-NG1 / finding §M2).
    /// `Some(node_id)` when the request reached this gateway via the
    /// ADR-042 §4 server-side proxy fallback — the value is the
    /// `NodeId` of the proxying node. The leader's audit record for
    /// this write MUST carry both the originating tenant (`tenant_id`)
    /// AND `forwarded_from_node` so an audit reviewer can distinguish
    /// a "client-direct" write from one routed via another gateway.
    /// `None` for client-direct writes (no proxy hop).
    pub forwarded_from_node: Option<u64>,
    /// Optional composition-id override (Group V #1 cross-protocol
    /// bridge). When `Some(id)`, the gateway uses `create_at(id, …)`
    /// instead of minting a fresh UUID — i.e. the resulting
    /// composition's id is exactly the supplied value.
    ///
    /// Used by the NFS data path's flush-on-COMMIT to keep the
    /// `composition_id` the NFS client returned at CREATE time
    /// (extracted from the synthetic `[comp_id 16][zeros 16]` file
    /// handle) consistent across the protocol boundary, so a later
    /// S3 GET on `bucket/{that_uuid}` finds the actual composition
    /// via the `comps.get(uuid)` fallback in `s3_server::get_object`.
    /// `None` for callers (S3 PUT, native, in-memory) that don't
    /// need a stable cross-protocol id.
    pub comp_id_override: Option<CompositionId>,
    /// Optional placement-tier hint (ADR-045 §D4/D5). The reserved
    /// names `fast` / `bulk` / `cold` steer the write onto a device
    /// class; `None` (or any other value) means fastest-fit. Carried on
    /// the existing chunk-store `pool` seam, so it propagates to every
    /// node's local placement through the EC/replication fan-out without
    /// changing durability placement. Per-protocol adapters populate it
    /// (S3 `x-amz-storage-class`, a native field, or a namespace default
    /// once namespace metadata is replicated — Phase 18 / ADR-045 §D3).
    pub tier: Option<String>,
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
    /// exists and currently maps to the given `composition_id`.
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

// #111: write forwarding is handled below the gateway by
// `kiseki_log::traits::AppendForwarder` (forward the built append to the
// shard leader's LogService), covering write/delete/multipart uniformly.
// The earlier gateway-level `WriteForwarder` (re-issue the whole write)
// was superseded — re-issuing the op can't work for multipart-complete
// (the upload's part-state is local to the origin node).

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

    /// ADR-042 §4 — `write` that surfaces
    /// [`GatewayError::ForwardToLeader`] when the local node is a
    /// follower for the target shard's Raft group.
    ///
    /// Default impl delegates to [`Self::write`], so backends that
    /// always behave as their own leader (in-process single-node,
    /// in-memory test gateways) need no change. Production
    /// gateways with multi-node Raft override this to call
    /// [`kiseki_log::traits::LogOps::append_chunk_and_delta_with_forwarding`]
    /// (and the matching `..._with_forwarding` sibling on
    /// `append_delta` paths) so the follower's hint reaches the
    /// caller as `GatewayError::ForwardToLeader { leader_node_id }`.
    ///
    /// The native `ServerImpl::put_object` proxy path
    /// (`KISEKI_NATIVE_PROXY_FALLBACK=on`) calls this method when
    /// proxy fallback is enabled; otherwise it falls back to
    /// `write` so the existing semantics are unchanged.
    async fn write_with_forwarding(
        &self,
        req: WriteRequest,
    ) -> Result<WriteResponse, GatewayError> {
        self.write(req).await
    }

    /// Force durability of all writes the gateway has accepted.
    ///
    /// Honors POSIX `fsync(2)` semantics under the group-commit
    /// optimization: when the persistent composition store runs at
    /// `Durability::None` (set via `KISEKI_COMPOSITION_FLUSH_INTERVAL_MS`)
    /// and the chunk store runs with `sync_per_write=false`, individual
    /// `write` calls return before the bytes hit stable storage. A
    /// FUSE / NFS client invoking `fsync(2)` needs an explicit
    /// "now please flush everything" signal — this RPC.
    ///
    /// Default: no-op (`Ok(())`). Persistent gateway impls override
    /// to drive `composition.flusher().flush()` plus
    /// `chunk_device.sync()`. The cost is one fsync per call;
    /// callers should batch (e.g. only invoke on actual `fsync(2)`,
    /// not on `close(2)`).
    async fn fsync_pending(&self) -> Result<(), GatewayError> {
        Ok(())
    }

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

    /// Resolve `(namespace_id, name)` → `composition_id` via the per-
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

    /// Drop ONLY a name→composition binding from the name index,
    /// leaving the composition (and its chunks) intact. Distinct from
    /// `delete_by_name`, which also deletes the composition. Used by the
    /// NFS rename path (#127) to retire the old name. Returns `true` if
    /// a binding existed.
    ///
    /// Default: `Ok(false)`.
    async fn unbind_object_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        let _ = (tenant_id, namespace_id, name);
        Ok(false)
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
        (**self).delete_by_name(tenant_id, namespace_id, name).await
    }
    async fn unbind_object_name(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        name: &str,
    ) -> Result<bool, GatewayError> {
        (**self)
            .unbind_object_name(tenant_id, namespace_id, name)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RED-first: I-NG5 (idempotency dedup byte-preserve through proxy)
    /// requires `WriteRequest` to carry the request's
    /// `ControlFields.idempotency_key`. Audit finding `I-NG5` in
    /// `specs/findings/2026-05-15-gate2-audit.md` calls this out as
    /// DEFERRED — the internal `WriteRequest` doesn't have the field, so
    /// the wire-level proxy can't preserve it byte-for-byte. Without
    /// the key on `WriteRequest`, retries through a different node will
    /// commit twice instead of dedup-short-circuiting on the leader.
    #[test]
    fn write_request_carries_idempotency_key() {
        let req = WriteRequest {
            tenant_id: OrgId(uuid::Uuid::nil()),
            namespace_id: NamespaceId(uuid::Uuid::nil()),
            data: b"payload".to_vec(),
            name: Some("k".into()),
            conditional: None,
            workflow_ref: None,
            idempotency_key: Some(vec![0xAB; 16]),
            forwarded_from_node: None,
            comp_id_override: None,
            tier: None,
        };
        let cloned = req.clone();
        assert_eq!(
            cloned.idempotency_key,
            Some(vec![0xAB; 16]),
            "idempotency_key MUST survive a clone so the proxy hop can re-issue it byte-for-byte"
        );
    }

    /// Audit I-NG1 / finding §M2: a proxy hop MUST surface
    /// `forwarded_from_node` on the leader's internal `WriteRequest`
    /// so the audit-record write attributes both originating tenant
    /// AND forwarding node. Validates struct-level support; the
    /// proto<->Rust thread is exercised by the wire-level test in
    /// `tests/proxy_wire.rs`.
    #[test]
    fn write_request_carries_forwarded_from_node() {
        let req = WriteRequest {
            tenant_id: OrgId(uuid::Uuid::nil()),
            namespace_id: NamespaceId(uuid::Uuid::nil()),
            data: b"payload".to_vec(),
            name: None,
            conditional: None,
            workflow_ref: None,
            idempotency_key: Some(vec![1, 2, 3]),
            forwarded_from_node: Some(7),
            comp_id_override: None,
            tier: None,
        };
        assert_eq!(req.clone().forwarded_from_node, Some(7));
    }
}
