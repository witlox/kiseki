//! Native-binding `GatewayOps` adapter — bridges `KisekiFuse` (and any
//! other `GatewayOps` consumer) to a running kiseki-server cluster
//! over the ADR-042 TCP-framed-postcard binding.
//!
//! Why a second `GatewayOps` impl alongside `RemoteHttpGateway`?
//!
//!   * `RemoteHttpGateway` rides the S3 listener (HTTP + reqwest +
//!     header-parsing tax). Single-node 64 KiB GET landed at ~7.6 k
//!     op/s on the perf harness.
//!   * The TCP-framed binding skips h2 / S3 framing entirely — the
//!     V3 wire format ships meta + bulk in one writev syscall and
//!     the server attaches bulk straight onto the typed request.
//!     Single-node 64 KiB GET measures ~70 k op/s on the same box.
//!   * FUSE is the canonical `GatewayOps` consumer; wiring it
//!     through this adapter pulls FUSE up to the native floor
//!     instead of paying the S3 transport tax twice (FUSE → HTTP
//!     → server-side S3 dispatch → in-process gateway).
//!
//! Limitations (deliberate, not future TODOs):
//!
//!   * Multipart methods all return `OperationNotSupported`. The
//!     native binding has no `upload_part` verb; only the S3
//!     listener accepts streaming uploads. FUSE does single-PUT
//!     writes so this isn't on the hot path.
//!   * `ensure_namespace` is a no-op. The server registers the
//!     bootstrap namespace on boot; multi-tenant namespace
//!     replication is the gateway's responsibility, not the
//!     client's.

#![cfg(feature = "native")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};

use crate::native::{TcpFramedClient, TcpFramedClientError};

/// Network-attached `GatewayOps` impl driving the ADR-042 TCP-framed
/// native binding. Holds a pool of independent connections so
/// concurrent FUSE / NFS / S3 callers fan out across server-side
/// reader tasks (one per connection on the server).
///
/// Cheap to clone: the `clients` Vec is `Arc<TcpFramedClient>` so
/// clones share the same connection pool. The round-robin counter
/// is also `Arc`-shared so all clones balance across the same set
/// of slots (no double-loading slot 0 by two clones starting fresh).
/// Lets callers (e.g. the FUSE perf driver) hold one instance for
/// direct `.await` calls AND hand a peer instance to a `KisekiFuse`
/// inode-table holder without duplicating the underlying TCP
/// connections.
pub struct NativeRemoteGateway {
    /// Round-robin pool. Each connection has its own server-side
    /// reader task; with N connections the server processes up to N
    /// in-flight requests in parallel. Pool size is taken from
    /// `KISEKI_NATIVE_GATEWAY_POOL` (default 16) at construction.
    clients: Vec<Arc<TcpFramedClient>>,
    /// Per-call selector — atomic so concurrent callers don't all
    /// hash to slot 0. `Arc`-wrapped so cheap clones share the
    /// counter.
    next: Arc<AtomicUsize>,
}

impl Clone for NativeRemoteGateway {
    fn clone(&self) -> Self {
        Self {
            clients: self.clients.clone(),
            next: Arc::clone(&self.next),
        }
    }
}

/// Default pool size when `KISEKI_NATIVE_GATEWAY_POOL` isn't set.
/// Matches the typical FUSE worker concurrency.
pub const DEFAULT_POOL_SIZE: usize = 16;

impl NativeRemoteGateway {
    /// Connect to a kiseki-server's TCP-framed listener at `addr`
    /// (e.g. `127.0.0.1:9300`). Establishes `pool` plaintext
    /// connections up-front; `pool` is clamped to a minimum of 1.
    ///
    /// Plaintext is the right default for the perf harness and
    /// single-node smoke tests. Production deployments override
    /// to TLS via [`connect_tls`].
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if any of the `pool` TCP
    /// connections fails — refusing to half-construct keeps the
    /// failure mode obvious (one connection error vs N "what is
    /// going on" connection errors at first call).
    pub async fn connect_plaintext(
        addr: impl tokio::net::ToSocketAddrs + Clone,
        pool: usize,
    ) -> std::io::Result<Self> {
        let pool = pool.max(1);
        let mut clients = Vec::with_capacity(pool);
        for _ in 0..pool {
            let c = TcpFramedClient::connect_plaintext(addr.clone()).await?;
            clients.push(c);
        }
        Ok(Self {
            clients,
            next: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn pick(&self) -> Arc<TcpFramedClient> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        Arc::clone(&self.clients[idx])
    }

    fn ctrl(tenant: OrgId) -> kiseki_proto::v1::native::ControlFields {
        kiseki_proto::v1::native::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: tenant.0.to_string(),
            }),
            // Per-call idempotency key so retries don't double-PUT.
            // FUSE today doesn't retry at this layer, but the server
            // still records the key for replay protection.
            idempotency_key: uuid::Uuid::new_v4().as_bytes().to_vec(),
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }
}

fn map_native_err(verb: &'static str, e: &TcpFramedClientError) -> GatewayError {
    GatewayError::ProtocolError(format!("native {verb}: {e}"))
}

#[async_trait::async_trait]
impl GatewayOps for NativeRemoteGateway {
    async fn read(&self, req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        // V3 GET: meta carries the typed request; bulk rides back as
        // the object data with no postcard pass.
        let proto_req = kiseki_proto::v1::native::GetObjectRequest {
            control: Some(Self::ctrl(req.tenant_id)),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: req.namespace_id.0.to_string(),
            }),
            range_start: req.offset,
            // range_end == 0 means "to EOF" per the server contract.
            // Translate `length == u64::MAX` (FUSE's "whole object"
            // sentinel) the same way.
            range_end: if req.length == u64::MAX {
                0
            } else {
                req.offset.saturating_add(req.length)
            },
            key: Some(
                kiseki_proto::v1::native::get_object_request::Key::CompositionId(
                    kiseki_proto::v1::CompositionId {
                        value: req.composition_id.0.to_string(),
                    },
                ),
            ),
        };
        let req_meta = postcard::to_allocvec(&proto_req)
            .map_err(|e| GatewayError::ProtocolError(format!("native read encode: {e}")))?;
        let (resp_meta, resp_bulk) = self
            .pick()
            .call_ok("get_object", req_meta, Vec::new())
            .await
            .map_err(|e| map_native_err("get_object", &e))?;
        let resp: kiseki_proto::v1::native::GetObjectResponse = postcard::from_bytes(&resp_meta)
            .map_err(|e| GatewayError::ProtocolError(format!("native read decode: {e}")))?;
        // EOF inference matches RemoteHttpGateway: if the server
        // returned fewer bytes than requested, EOF.
        let eof = req.length == u64::MAX
            || u64::try_from(resp_bulk.len()).unwrap_or(u64::MAX) < req.length;
        let content_type = if resp.content_type.is_empty() {
            None
        } else {
            Some(resp.content_type)
        };
        Ok(ReadResponse {
            data: resp_bulk,
            eof,
            content_type,
        })
    }

    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        // V3 PUT: meta carries the typed request with `data` empty;
        // the actual payload rides as bulk.
        let bytes_written = req.data.len() as u64;
        // Server requires a non-empty name for the binding index.
        // FUSE callers pass `None`; mint a UUID so the put still
        // routes through the named-PUT path uniformly.
        let name = req.name.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let proto_req = kiseki_proto::v1::native::PutObjectRequest {
            control: Some(Self::ctrl(req.tenant_id)),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: req.namespace_id.0.to_string(),
            }),
            name,
            data: Vec::new(),
        };
        let req_meta = postcard::to_allocvec(&proto_req)
            .map_err(|e| GatewayError::ProtocolError(format!("native write encode: {e}")))?;
        let (resp_meta, _resp_bulk) = self
            .pick()
            .call_ok("put_object", req_meta, req.data)
            .await
            .map_err(|e| map_native_err("put_object", &e))?;
        let resp: kiseki_proto::v1::native::PutObjectResponse = postcard::from_bytes(&resp_meta)
            .map_err(|e| GatewayError::ProtocolError(format!("native write decode: {e}")))?;
        let comp = resp.composition_id.ok_or_else(|| {
            GatewayError::ProtocolError("native write: missing composition_id".into())
        })?;
        let composition_id = uuid::Uuid::parse_str(&comp.value)
            .map(CompositionId)
            .map_err(|_| {
                GatewayError::ProtocolError(format!(
                    "native write: composition_id is not a UUID: {}",
                    comp.value
                ))
            })?;
        Ok(WriteResponse {
            composition_id,
            bytes_written,
        })
    }

    async fn delete(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        composition_id: CompositionId,
    ) -> Result<(), GatewayError> {
        let proto_req = kiseki_proto::v1::native::DeleteObjectRequest {
            control: Some(Self::ctrl(tenant_id)),
            namespace_id: Some(kiseki_proto::v1::NamespaceId {
                value: namespace_id.0.to_string(),
            }),
            composition_id: Some(kiseki_proto::v1::CompositionId {
                value: composition_id.0.to_string(),
            }),
        };
        let req_meta = postcard::to_allocvec(&proto_req)
            .map_err(|e| GatewayError::ProtocolError(format!("native delete encode: {e}")))?;
        let _ = self
            .pick()
            .call_ok("delete_object", req_meta, Vec::new())
            .await
            .map_err(|e| map_native_err("delete_object", &e))?;
        Ok(())
    }
}
