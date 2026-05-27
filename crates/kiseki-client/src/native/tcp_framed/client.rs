//! `TcpFramedClient` — client-side connection for ADR-042 §2.2.
//!
//! Lifecycle:
//! 1. [`TcpFramedClient::connect_plaintext`] / `connect_tls` —
//!    establish a TCP (+ optional rustls) connection.
//! 2. Internally split the stream into `(read_half, write_half)`.
//!    Spawn a reader task that owns `read_half` and drives the
//!    response demux loop. The client struct owns
//!    `Mutex<write_half>` and the pending-request map.
//! 3. [`TcpFramedClient::call`] — allocate `request_id`, register a
//!    `oneshot::Sender` in the pending map, write the request frame,
//!    await the `oneshot::Receiver`.
//! 4. Drop semantics — when the client is dropped, the writer half
//!    closes, the server's read EOFs, the connection closes
//!    cleanly. Pending requests resolve with
//!    `TcpFramedClientError::ConnectionClosed`.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use kiseki_proto::native_contract::wire_tcp_framed::{
    build_request_header, decode_response_frame, validate_frame_length, WireDecodeError, WireStatus,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

/// Configure `SO_LINGER = 0` so a `close(2)` on the socket — including
/// the implicit close that happens when the kiseki-client process is
/// SIGKILL'd — sends RST instead of FIN. Trades any in-flight bytes
/// (the connection is being torn down anyway) for immediate cleanup.
///
/// 2026-05-09 GCP finding: without this, a SIGKILL'd kiseki-client
/// leaves up to `pool` sockets in `LAST-ACK` for ~`tcp_fin_timeout`
/// (60 s default Linux). The next kiseki-client restart's connect
/// loop hits the server's per-peer-cap before the LAST-ACK sockets
/// drop and the new pool can't fully establish. Pinning lives in
/// `crates/kiseki-client/tests/per_peer_cap_collision.rs::cap_is_enforced_at_listener_layer`
/// — bumping the cap is the structural fix; this `linger=0` is the
/// per-socket fast-clean fix that makes restarts fast even within
/// the cap.
fn configure_close_linger(stream: &tokio::net::TcpStream) {
    // `set_linger` is `#[deprecated]` in tokio because non-zero
    // linger durations block the closing thread. We pass
    // `Duration::ZERO`, which makes `close(2)` non-blocking AND
    // sends RST instead of FIN — exactly the behavior we want for
    // a daemon-restart fast-cleanup path. The deprecation is
    // defensive over-warning for a different use case; suppress.
    #[allow(deprecated)]
    let r = stream.set_linger(Some(Duration::ZERO));
    if let Err(e) = r {
        tracing::warn!(error = %e, "TCP-framed client: SO_LINGER=0 failed; restart cleanup may be slow");
    }
}

/// Errors raised by [`TcpFramedClient`].
#[derive(Debug, thiserror::Error)]
pub enum TcpFramedClientError {
    /// I/O error on the underlying TCP/TLS connection.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Wire-level decode error (oversize length, version mismatch,
    /// malformed envelope) — connection torn down.
    #[error("wire decode: {0}")]
    WireDecode(#[from] WireDecodeError),
    /// Connection closed while the request was pending. Caller can
    /// reconnect and retry idempotent operations.
    #[error("connection closed before response received")]
    ConnectionClosed,
    /// Server returned a status with a reason payload. The variant
    /// preserves both for callers that want to map back to a typed
    /// error (e.g. `NativeError`); most callers just want the
    /// reason string for logging.
    #[error("server returned {status:?}: {reason}")]
    ServerError {
        /// Wire status byte from the response.
        status: WireStatus,
        /// UTF-8 reason string from the response payload.
        reason: String,
    },
}

/// V3 response payload sent through the oneshot — status + meta +
/// bulk. Bulk is empty for non-bulk verbs and for error responses.
pub type CallResponse = (WireStatus, Vec<u8>, Vec<u8>);

/// Pending requests keyed by `request_id`. The writer side inserts;
/// the reader task removes + signals with `CallResponse`.
type PendingMap = DashMap<u64, oneshot::Sender<CallResponse>>;

/// Client-side TCP-framed-postcard connection. Cheap to clone via
/// `Arc<TcpFramedClient>` for shared use across tasks.
///
/// One connection per `(client, node)`; pipelining via `request_id`
/// correlation lets multiple `call` invocations be in flight
/// concurrently on the same connection.
pub struct TcpFramedClient {
    write_half: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Arc<PendingMap>,
    next_request_id: AtomicU64,
    /// Reader task handle. Aborted on drop.
    reader_task: JoinHandle<()>,
}

impl TcpFramedClient {
    /// Build a client over an already-established connection. The
    /// caller has done the TCP connect + TLS handshake (or chose
    /// plaintext for dev mode); we own the stream from here on.
    fn from_stream<S>(stream: S) -> Arc<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (read_half, write_half) = tokio::io::split(stream);
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let pending_for_reader = Arc::clone(&pending);
        let reader_task = tokio::spawn(reader_loop(read_half, pending_for_reader));
        Arc::new(Self {
            write_half: AsyncMutex::new(Box::new(write_half)),
            pending,
            next_request_id: AtomicU64::new(1),
            reader_task,
        })
    }

    /// Connect to a kiseki-server's TCP-framed listener in plaintext
    /// mode. Use only for dev; production deployments require TLS
    /// and the listener rejects plaintext at startup.
    ///
    /// # Errors
    /// Returns `io::Error` from the TCP connect step.
    pub async fn connect_plaintext(addr: impl tokio::net::ToSocketAddrs) -> io::Result<Arc<Self>> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        // TCP_NODELAY — see listener-side comment. The split-write
        // (header then body) hits a Nagle 40 ms timer without it.
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(error = %e, "TCP-framed client: TCP_NODELAY failed; perf may regress");
        }
        configure_close_linger(&stream);
        Ok(Self::from_stream(stream))
    }

    /// Connect to a kiseki-server's TCP-framed listener over rustls.
    /// `tls_config` MUST be configured with the cluster CA as the
    /// trust root (same chain the gRPC binding uses) and a client
    /// cert whose SAN matches the tenant the caller intends to act
    /// as (§5 SAN canonicalization).
    ///
    /// # Errors
    /// Returns `io::Error` on TCP connect or TLS handshake failure.
    pub async fn connect_tls(
        addr: impl tokio::net::ToSocketAddrs,
        server_name: rustls::pki_types::ServerName<'static>,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> io::Result<Arc<Self>> {
        let tcp = tokio::net::TcpStream::connect(addr).await?;
        configure_close_linger(&tcp);
        if let Err(e) = tcp.set_nodelay(true) {
            tracing::warn!(error = %e, "TCP-framed client (TLS): TCP_NODELAY failed");
        }
        let connector = tokio_rustls::TlsConnector::from(tls_config);
        let tls = connector.connect(server_name, tcp).await?;
        Ok(Self::from_stream(tls))
    }

    /// Issue one V3 RPC. Returns `(status, meta, bulk)` matching the
    /// V3 wire layout. `meta` is the postcard-encoded response struct
    /// (with bulk fields empty); `bulk` is the raw bulk-field bytes
    /// for response-bulk verbs (`get_object`, `read`) or empty
    /// otherwise. On non-Ok statuses, `meta` is a UTF-8 reason
    /// string and `bulk` is empty.
    ///
    /// `req_meta`: postcard-encoded request struct (with the
    /// request's bulk field — if any — set to empty/None).
    /// `req_bulk`: raw bulk bytes for request-bulk verbs
    /// (`put_object`, `write`); empty for non-bulk verbs.
    ///
    /// # Errors
    /// `Io` / `WireDecode` / `ConnectionClosed`. Non-Ok statuses do
    /// NOT error here — they ride through `meta`; use [`call_ok`] for
    /// the convenience wrapping.
    pub async fn call(
        &self,
        verb_tag: &str,
        req_meta: Vec<u8>,
        req_bulk: Vec<u8>,
    ) -> Result<CallResponse, TcpFramedClientError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(request_id, tx);

        // Build the small request header (length prefix + version +
        // request_id + verb_tag + meta_len). Vectored write below
        // ships [header, meta, bulk] in one syscall.
        let header =
            match build_request_header(request_id, verb_tag, req_meta.len(), req_bulk.len()) {
                Ok(h) => h,
                Err(e) => {
                    self.pending.remove(&request_id);
                    return Err(TcpFramedClientError::Io(io::Error::other(e.to_string())));
                }
            };
        {
            let mut guard = self.write_half.lock().await;
            if let Err(e) =
                write_request_vectored3(&mut *guard, &header, &req_meta, &req_bulk).await
            {
                self.pending.remove(&request_id);
                return Err(e.into());
            }
            if let Err(e) = guard.flush().await {
                self.pending.remove(&request_id);
                return Err(e.into());
            }
        }

        let (status, meta, bulk) = rx
            .await
            .map_err(|_| TcpFramedClientError::ConnectionClosed)?;
        Ok((status, meta, bulk))
    }

    /// Convenience: like [`call`] but maps non-Ok statuses to
    /// `ServerError`. Returns `(meta, bulk)` on Ok.
    ///
    /// # Errors
    /// Same as [`call`], plus `ServerError` for non-Ok status.
    ///
    /// [`call`]: Self::call
    pub async fn call_ok(
        &self,
        verb_tag: &str,
        req_meta: Vec<u8>,
        req_bulk: Vec<u8>,
    ) -> Result<(Vec<u8>, Vec<u8>), TcpFramedClientError> {
        let (status, meta, bulk) = self.call(verb_tag, req_meta, req_bulk).await?;
        if status == WireStatus::Ok {
            Ok((meta, bulk))
        } else {
            Err(TcpFramedClientError::ServerError {
                status,
                reason: String::from_utf8_lossy(&meta).into_owned(),
            })
        }
    }
}

impl Drop for TcpFramedClient {
    fn drop(&mut self) {
        // Aborting the reader task closes its read-half drop, which
        // (combined with the write_half being owned by `self`) ends
        // the connection cleanly. Any still-pending `oneshot` senders
        // get dropped; their receivers return Err which `call`
        // surfaces as `ConnectionClosed`.
        self.reader_task.abort();
    }
}

/// Reader task: pull frames off the read half, demultiplex by
/// `request_id`, signal the matching `oneshot::Sender`. Loop ends
/// on EOF, oversize/version error, or any I/O error — pending
/// requests then drop with `ConnectionClosed`.
/// `writev`-style 3-buffer write — header + meta + bulk in one
/// syscall. Falls back to sequential `write_all`s if the underlying
/// stream's vectored impl returns 0.
async fn write_request_vectored3<S: AsyncWrite + Unpin + ?Sized>(
    stream: &mut S,
    header: &[u8],
    meta: &[u8],
    bulk: &[u8],
) -> std::io::Result<()> {
    use std::io::IoSlice;
    use std::pin::Pin;
    use tokio::io::AsyncWriteExt;

    let mut offs = [0usize, 0, 0];
    let lens = [header.len(), meta.len(), bulk.len()];
    let total = lens.iter().sum::<usize>();
    let mut written = 0usize;
    while written < total {
        let bufs: [IoSlice<'_>; 3] = [
            IoSlice::new(&header[offs[0]..]),
            IoSlice::new(&meta[offs[1]..]),
            IoSlice::new(&bulk[offs[2]..]),
        ];
        let n = std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_write_vectored(cx, &bufs))
            .await?;
        if n == 0 {
            stream.write_all(&header[offs[0]..]).await?;
            if offs[1] < meta.len() {
                stream.write_all(&meta[offs[1]..]).await?;
            }
            if offs[2] < bulk.len() {
                stream.write_all(&bulk[offs[2]..]).await?;
            }
            return Ok(());
        }
        written += n;
        let mut remaining = n;
        for i in 0..3 {
            let avail = lens[i] - offs[i];
            if remaining == 0 {
                break;
            }
            if remaining <= avail {
                offs[i] += remaining;
                remaining = 0;
            } else {
                offs[i] = lens[i];
                remaining -= avail;
            }
        }
    }
    Ok(())
}

async fn reader_loop<R>(mut read_half: R, pending: Arc<PendingMap>)
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    while read_half.read_exact(&mut len_buf).await.is_ok() {
        let length_be = u32::from_be_bytes(len_buf);
        let Ok(body_len) = validate_frame_length(length_be) else {
            break;
        };
        // Single read of the whole frame body. The V3 wire format's
        // bulk-bytes-around-postcard split means we're already
        // skipping the body's postcard encode/decode on each side;
        // the `to_vec()`s below copy the meta + bulk slices out of
        // the read buffer once each so they can ride through the
        // oneshot. Caller (`call`) reattaches `bulk` onto the typed
        // verb response WITHOUT another postcard pass.
        let mut body = vec![0u8; body_len];
        if read_half.read_exact(&mut body).await.is_err() {
            break;
        }
        let Ok(view) = decode_response_frame(&body) else {
            break;
        };
        if let Some((_, sender)) = pending.remove(&view.request_id) {
            let _ = sender.send((view.status, view.meta.to_vec(), view.bulk.to_vec()));
        }
        // Unmatched response (server bug): drop silently.
    }
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Test helper: read one V3 request frame from the duplex
    /// stream. Returns (`request_id`, `verb_tag`, `meta`, `bulk`).
    async fn read_v3_request<S: AsyncRead + Unpin>(
        stream: &mut S,
    ) -> (u64, String, Vec<u8>, Vec<u8>) {
        use kiseki_proto::native_contract::wire_tcp_framed::decode_request_frame;
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let body_len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.unwrap();
        let view = decode_request_frame(&body).expect("decode req");
        (
            view.request_id,
            view.verb_tag.to_string(),
            view.meta.to_vec(),
            view.bulk.to_vec(),
        )
    }

    /// Minimal in-memory loopback test: spawn a fake server that
    /// reads one frame and echoes a response, run the client
    /// against it.
    #[tokio::test]
    async fn call_round_trips_through_in_memory_loopback() {
        use kiseki_proto::native_contract::wire_tcp_framed::encode_response_frame;
        let (client_side, mut server_side) = duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            let (request_id, verb, meta, bulk) = read_v3_request(&mut server_side).await;
            assert_eq!(verb, "ping");
            assert_eq!(meta, b"hello".to_vec());
            assert!(bulk.is_empty());

            let resp_frame =
                encode_response_frame(WireStatus::Ok, request_id, b"world", &[]).unwrap();
            server_side.write_all(&resp_frame).await.unwrap();
            server_side.flush().await.unwrap();
        });

        let client = TcpFramedClient::from_stream(client_side);
        let (status, meta, bulk) = client
            .call("ping", b"hello".to_vec(), Vec::new())
            .await
            .unwrap();
        assert_eq!(status, WireStatus::Ok);
        assert_eq!(meta, b"world");
        assert!(bulk.is_empty());

        server_task.await.unwrap();
    }

    /// `call_ok` happy path returns the payload directly.
    #[tokio::test]
    async fn call_ok_returns_payload_on_success() {
        use kiseki_proto::native_contract::wire_tcp_framed::encode_response_frame;
        let (client_side, mut server_side) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let (request_id, _verb, _meta, _bulk) = read_v3_request(&mut server_side).await;
            let frame = encode_response_frame(WireStatus::Ok, request_id, &[1, 2, 3], &[]).unwrap();
            server_side.write_all(&frame).await.unwrap();
        });
        let client = TcpFramedClient::from_stream(client_side);
        let (meta, bulk) = client.call_ok("v", Vec::new(), Vec::new()).await.unwrap();
        assert_eq!(meta, vec![1, 2, 3]);
        assert!(bulk.is_empty());
        server_task.await.unwrap();
    }

    /// `call_ok` maps non-Ok statuses to `ServerError` with the
    /// reason payload as a UTF-8 string.
    #[tokio::test]
    async fn call_ok_maps_non_ok_status_to_server_error() {
        use kiseki_proto::native_contract::wire_tcp_framed::encode_response_frame;
        let (client_side, mut server_side) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let (request_id, _verb, _meta, _bulk) = read_v3_request(&mut server_side).await;
            let frame = encode_response_frame(
                WireStatus::PermissionDenied,
                request_id,
                b"san_payload_tenant_mismatch",
                &[],
            )
            .unwrap();
            server_side.write_all(&frame).await.unwrap();
        });
        let client = TcpFramedClient::from_stream(client_side);
        let err = client
            .call_ok("v", Vec::new(), Vec::new())
            .await
            .expect_err("non-Ok status");
        match err {
            TcpFramedClientError::ServerError { status, reason } => {
                assert_eq!(status, WireStatus::PermissionDenied);
                assert!(reason.contains("san_payload_tenant_mismatch"));
            }
            other => panic!("expected ServerError, got: {other:?}"),
        }
        server_task.await.unwrap();
    }

    /// Pipelined calls — issue two `call` futures concurrently;
    /// reader demuxes by `request_id` so each future gets its own
    /// response. Confirms multiplex actually works.
    #[tokio::test]
    async fn pipelined_calls_demultiplex_by_request_id() {
        use kiseki_proto::native_contract::wire_tcp_framed::encode_response_frame;
        let (client_side, mut server_side) = duplex(64 * 1024);

        // Server: read both requests, then write responses in
        // REVERSE order to prove the client demuxes by request_id
        // rather than by arrival order.
        let server_task = tokio::spawn(async move {
            let mut request_ids = Vec::new();
            for _ in 0..2 {
                let (rid, _verb, _meta, _bulk) = read_v3_request(&mut server_side).await;
                request_ids.push(rid);
            }
            for rid in request_ids.into_iter().rev() {
                let payload = format!("rid={rid}").into_bytes();
                let frame = encode_response_frame(WireStatus::Ok, rid, &payload, &[]).unwrap();
                server_side.write_all(&frame).await.unwrap();
            }
            server_side.flush().await.unwrap();
        });

        let client = TcpFramedClient::from_stream(client_side);
        let c1 = Arc::clone(&client);
        let c2 = Arc::clone(&client);
        let f1 = tokio::spawn(async move { c1.call_ok("a", Vec::new(), Vec::new()).await });
        let f2 = tokio::spawn(async move { c2.call_ok("b", Vec::new(), Vec::new()).await });
        let (m1, _b1) = f1.await.unwrap().unwrap();
        let (m2, _b2) = f2.await.unwrap().unwrap();
        assert_eq!(String::from_utf8(m1).unwrap(), "rid=1");
        assert_eq!(String::from_utf8(m2).unwrap(), "rid=2");
        server_task.await.unwrap();
    }

    /// Connection closed mid-call → `ConnectionClosed` error. Drops
    /// the server side, the reader task EOFs, all pending oneshots
    /// drop, the call returns `ConnectionClosed`.
    #[tokio::test]
    async fn pending_call_returns_connection_closed_on_server_drop() {
        let (client_side, server_side) = duplex(64 * 1024);
        let client = TcpFramedClient::from_stream(client_side);
        let c = Arc::clone(&client);
        let call_task = tokio::spawn(async move { c.call("v", Vec::new(), Vec::new()).await });
        // Give the writer a moment to enqueue the request.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(server_side);
        let result = call_task.await.unwrap();
        match result {
            Err(TcpFramedClientError::ConnectionClosed) => {}
            other => panic!("expected ConnectionClosed, got: {other:?}"),
        }
    }

    /// End-to-end through the real server-side `serve_connection` (in
    /// the kiseki-gateway crate). Confirms the wire format matches
    /// on both sides — the highest-leverage test in this slice.
    #[tokio::test]
    async fn end_to_end_against_real_server_serve_connection() {
        use kiseki_chunk::ChunkStore;
        use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
        use kiseki_common::tenancy::KeyEpoch;
        use kiseki_composition::composition::CompositionStore;
        use kiseki_composition::namespace::Namespace;
        use kiseki_crypto::keys::SystemMasterKey;
        use kiseki_gateway::mem_gateway::InMemoryGateway;
        use kiseki_gateway::native::server::ServerImpl;
        use kiseki_gateway::native::signing_keys::SigningKeys;
        use kiseki_gateway::native::tcp_framed::{
            serve_connection as server_serve, TcpFramedPrincipal,
        };
        use kiseki_proto::native_contract::ConnectionId;
        use kiseki_proto::v1;
        use kiseki_proto::v1::native as np;

        let gw = Arc::new(InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        ));
        gw.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_bytes([2; 16])),
            tenant_id: OrgId(uuid::Uuid::from_bytes([1; 16])),
            shard_id: ShardId(uuid::Uuid::nil()),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
            tier_policy: Vec::new(),
        })
        .await;
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        let server = Arc::new(ServerImpl::new(
            gw as Arc<dyn kiseki_gateway::ops::GatewayOps>,
            signing,
        ));

        let (client_side, mut server_side) = duplex(64 * 1024);
        let principal = TcpFramedPrincipal::new("", ConnectionId(1));
        let server_task = tokio::spawn(async move {
            let _ = server_serve(&mut server_side, server, principal).await;
        });

        let client = TcpFramedClient::from_stream(client_side);
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        // V3: meta = postcard(PutObjectRequest with empty .data),
        // bulk = the actual object data bytes.
        let req_meta = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(np::ControlFields {
                tenant_id: Some(v1::OrgId {
                    value: tenant.0.to_string(),
                }),
                idempotency_key: vec![0xAB; 8],
                workflow_ref: String::new(),
                cache_hint: None,
                conditional: None,
                forwarded_from_node: None,
            }),
            namespace_id: Some(v1::NamespaceId {
                value: ns.0.to_string(),
            }),
            name: "alpha".into(),
            data: Vec::new(),
        })
        .unwrap();
        let req_bulk = b"hello".to_vec();

        let (resp_meta, resp_bulk) = client
            .call_ok("put_object", req_meta, req_bulk)
            .await
            .unwrap();
        // PutObject response has no bulk.
        assert!(resp_bulk.is_empty());
        let put_resp: np::PutObjectResponse = postcard::from_bytes(&resp_meta).unwrap();
        assert_eq!(put_resp.size, 5);
        assert!(put_resp.composition_id.is_some());

        drop(client);
        let _ = server_task.await;
    }
}
