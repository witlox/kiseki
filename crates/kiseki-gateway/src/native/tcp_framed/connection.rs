//! Per-connection frame read/dispatch/write loop for the TCP-framed
//! binding (ADR-042 §2.2).
//!
//! Lifecycle:
//! 1. Caller (the listener) hands us an already-handshaked stream
//!    (rustls or plaintext) plus the [`TcpFramedPrincipal`] minted
//!    from the validated peer cert.
//! 2. We loop: read `[length be u32][body]`, decode the V3 frame via
//!    [`decode_request_frame`], dispatch the verb via
//!    [`super::dispatch::dispatch_verb`], encode the result via
//!    [`encode_response_frame`], write back.
//! 3. Loop ends on read EOF, write error, or unrecoverable wire-level
//!    error (oversize, version mismatch).
//!
//! Concurrency: `request_id` multiplex within one connection is
//! deferred to slice 4 (client-side). v1 server processes frames
//! sequentially per-connection; the client may pipeline (multiple
//! outstanding requests on one connection) since the server preserves
//! request_id in the response envelope. Cross-connection concurrency
//! comes from the listener spawning one task per accepted connection.

use std::sync::Arc;

use kiseki_proto::native_contract::wire_tcp_framed::{
    build_response_header, decode_request_frame, encode_response_frame,
    validate_frame_length, WireDecodeError, WireStatus,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::native::server::ServerImpl;

use super::dispatch::dispatch_verb;
use super::principal::TcpFramedPrincipal;

/// Maximum bytes to drain on a malformed-frame error before tearing
/// down the connection. Bounded so a peer can't OOM us by sending an
/// oversize length and then never closing.
const ERROR_DRAIN_CAP: usize = 16 * 1024;

/// Errors that end a connection. The wire-level subset is mapped to
/// a response frame BEFORE we close, so the peer learns why; I/O
/// errors close immediately (we can't write back through a broken
/// pipe).
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    /// Read/write I/O error — connection torn down without sending
    /// a response.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Wire-level decode failure on a length prefix or version byte
    /// — the peer sent something we can't reply to coherently.
    #[error("wire decode: {0}")]
    WireDecode(#[from] WireDecodeError),
}

/// Run the request/response loop on one already-handshaked connection.
///
/// `principal` carries the canonical SAN extracted from the peer
/// cert at the rustls handshake. Loop ends cleanly on EOF (peer
/// closed) or with [`ConnectionError`] on I/O / wire-level failure.
///
/// # Errors
/// I/O failure on the stream, or unrecoverable wire-level decode
/// error. Per-frame errors (verb-level, oversize body, version
/// mismatch) are converted to response frames sent back over the
/// wire, NOT propagated up here.
pub async fn serve_connection<S>(
    stream: &mut S,
    server: Arc<ServerImpl>,
    principal: TcpFramedPrincipal,
) -> Result<(), ConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut len_buf = [0u8; 4];
    loop {
        // Length prefix — if read returns 0 bytes, peer closed cleanly.
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let length_be = u32::from_be_bytes(len_buf);
        let body_len = match validate_frame_length(length_be) {
            Ok(n) => n,
            Err(e) => {
                // Oversize length is unrecoverable; the peer's read
                // stream is now skewed by `length_be` bytes we won't
                // drain. Send a wire-level error response (request_id
                // 0 — we never decoded a frame header) then close.
                let payload = format!("frame oversize: {length_be} > cap").into_bytes();
                let _ =
                    write_response(stream, WireStatus::ProtocolError, 0, &payload, &[]).await;
                return Err(e.into());
            }
        };

        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await?;

        // Decode V3 frame header — borrowed view over `body`.
        // `meta` is the postcard-encoded request metadata; `bulk`
        // is the raw bulk bytes (empty for non-bulk verbs). No copy
        // of either is needed at this layer; dispatch sees borrowed
        // slices.
        let (request_id, verb_tag, req_meta_off, req_bulk_off) =
            match decode_request_frame(&body) {
                Ok(view) => {
                    // Capture offsets so we can re-slice `body` for
                    // the dispatch call (the borrow checker won't
                    // let us hold `view` across the await — it
                    // borrows `body` and `body` is moved into the
                    // dispatch path).
                    let verb = view.verb_tag.to_string();
                    let meta_start = body.len() - view.meta.len() - view.bulk.len();
                    let bulk_start = body.len() - view.bulk.len();
                    (view.request_id, verb, meta_start, bulk_start)
                }
                Err(e) => {
                    let payload = format!("frame decode failed: {e}").into_bytes();
                    write_response(stream, WireStatus::ProtocolError, 0, &payload, &[])
                        .await?;
                    continue;
                }
            };

        let (status, resp_meta, resp_bulk) = dispatch_verb(
            &server,
            &principal,
            &verb_tag,
            &body[req_meta_off..req_bulk_off],
            &body[req_bulk_off..],
        )
        .await;
        write_response(stream, status, request_id, &resp_meta, &resp_bulk).await?;
    }
}

/// Write a V3 response frame to the wire via **3-iovec vectored
/// I/O** — the 18-byte header, the postcard `meta_bytes`, and the
/// raw `bulk_bytes` all go to the kernel in a single `writev`
/// syscall. Critical: bulk bytes are NEVER postcard-encoded; the
/// hot path for 64 KiB GET ships them straight from the chunk
/// store buffer to the wire, no per-byte serialize loop.
///
/// Falls back to sequential `write_all`s if the runtime's
/// `poll_write_vectored` returns 0 (some `AsyncWrite` adapters
/// don't implement vectored).
async fn write_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: WireStatus,
    request_id: u64,
    meta: &[u8],
    bulk: &[u8],
) -> std::io::Result<()> {
    let header = match build_response_header(status, request_id, meta.len(), bulk.len()) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "response frame header build failed; sending Internal");
            return write_response_oversize(stream, request_id).await;
        }
    };
    write_all_vectored3(stream, &header, meta, bulk).await?;
    stream.flush().await
}

/// `writev`-style 3-buffer write — drains [header, meta, bulk] via
/// `poll_write_vectored` in a loop, falling back to sequential
/// `write_all` if vectored returns 0.
async fn write_all_vectored3<S: AsyncWrite + Unpin>(
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
        // Consume bytes from header → meta → bulk in order.
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

/// Last-resort fallback when the response body would exceed the
/// frame cap — emits a tiny `Internal{response too large}` frame
/// so the peer's call returns an error rather than hanging.
async fn write_response_oversize<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request_id: u64,
) -> std::io::Result<()> {
    let frame = encode_response_frame(
        WireStatus::Internal,
        request_id,
        b"response too large",
        &[],
    )
    .map_err(|_| std::io::Error::other("response oversize fallback also too large"))?;
    stream.write_all(&frame).await?;
    stream.flush().await
}

/// Drain up to [`ERROR_DRAIN_CAP`] bytes from a stream — used after
/// a wire-level error to consume any half-written follow-up before
/// closing. Bounded so a malicious peer can't keep us reading.
#[allow(dead_code)] // reserved for future per-error recovery paths
async fn drain_for_close<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    while total < ERROR_DRAIN_CAP {
        match stream.read(&mut buf).await {
            Ok(0) => return Ok(()),
            Ok(n) => total += n,
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::native::signing_keys::SigningKeys;
    use kiseki_chunk::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_proto::native_contract::wire_tcp_framed::{
        encode_request_frame, NATIVE_TCP_FRAMED_VERSION_V3,
    };
    use kiseki_proto::native_contract::ConnectionId;
    use kiseki_proto::v1 as v1;
    use kiseki_proto::v1::native as np;
    use tokio::io::duplex;

    async fn make_server() -> Arc<ServerImpl> {
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
        })
        .await;
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        Arc::new(ServerImpl::new(
            gw as Arc<dyn crate::ops::GatewayOps>,
            signing,
        ))
    }

    fn ctrl(tenant: OrgId) -> np::ControlFields {
        np::ControlFields {
            tenant_id: Some(v1::OrgId {
                value: tenant.0.to_string(),
            }),
            idempotency_key: vec![0xAB; 8],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }

    fn ns_proto(ns: NamespaceId) -> v1::NamespaceId {
        v1::NamespaceId {
            value: ns.0.to_string(),
        }
    }

    /// Test-only response frame reader. Returns
    /// (status, request_id, meta, bulk) — V3 split-bulk shape.
    async fn read_response_frame<S: AsyncRead + Unpin>(
        stream: &mut S,
    ) -> (WireStatus, u64, Vec<u8>, Vec<u8>) {
        use kiseki_proto::native_contract::wire_tcp_framed::decode_response_frame;
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.expect("read length");
        let body_len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.expect("read body");
        let view = decode_response_frame(&body).expect("decode response frame");
        (view.status, view.request_id, view.meta.to_vec(), view.bulk.to_vec())
    }

    /// End-to-end: client sends a put_object request frame; the
    /// server's serve_connection processes it, replies, and the
    /// response decodes back to a real PutObjectResponse.
    #[tokio::test]
    async fn put_object_end_to_end_via_serve_connection() {
        let server = make_server().await;
        let principal = TcpFramedPrincipal::new("", ConnectionId(1));

        let (mut client_side, mut server_side) = duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            serve_connection(&mut server_side, server, principal).await
        });

        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        // V3: meta = postcard(PutObjectRequest with empty data),
        // bulk = the actual data bytes.
        let meta = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "alpha".into(),
            data: Vec::new(),
        })
        .unwrap();
        let bulk = b"hello";
        let req_frame = encode_request_frame(7, "put_object", &meta, bulk).unwrap();
        client_side.write_all(&req_frame).await.unwrap();
        client_side.flush().await.unwrap();

        let (status, request_id, resp_meta, resp_bulk) =
            read_response_frame(&mut client_side).await;
        assert_eq!(status, WireStatus::Ok);
        assert_eq!(request_id, 7, "request_id must echo");
        assert!(resp_bulk.is_empty(), "PutObject has no response bulk");
        let put_resp: np::PutObjectResponse = postcard::from_bytes(&resp_meta).unwrap();
        assert_eq!(put_resp.size, 5);

        drop(client_side);
        let result = server_task.await.expect("task joined");
        assert!(result.is_ok(), "serve_connection ended with: {result:?}");
    }

    /// Pipelined requests: client writes two frames before reading
    /// any response. Server processes them in order and replies on
    /// each — request_id correlation lets the client demultiplex.
    #[tokio::test]
    async fn pipelined_requests_round_trip_in_order() {
        let server = make_server().await;
        let principal = TcpFramedPrincipal::new("", ConnectionId(1));

        let (mut client_side, mut server_side) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(&mut server_side, server, principal).await
        });

        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        for &(rid, name) in &[(11u64, "a"), (22u64, "b")] {
            let meta = postcard::to_allocvec(&np::PutObjectRequest {
                control: Some(ctrl(tenant)),
                namespace_id: Some(ns_proto(ns)),
                name: name.into(),
                data: Vec::new(),
            })
            .unwrap();
            let frame = encode_request_frame(rid, "put_object", &meta, b"x").unwrap();
            client_side.write_all(&frame).await.unwrap();
        }
        client_side.flush().await.unwrap();

        for &want_rid in &[11u64, 22u64] {
            let (status, request_id, _, _) = read_response_frame(&mut client_side).await;
            assert_eq!(status, WireStatus::Ok);
            assert_eq!(request_id, want_rid);
        }
        drop(client_side);
        let _ = server_task.await;
    }

    /// Malformed envelope → ProtocolError response, but the
    /// connection stays open and the next valid frame still
    /// processes. Important for client robustness: a single bad
    /// frame doesn't sever the channel.
    #[tokio::test]
    async fn malformed_frame_keeps_connection_alive() {
        let server = make_server().await;
        let principal = TcpFramedPrincipal::new("", ConnectionId(1));
        let (mut client_side, mut server_side) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(&mut server_side, server, principal).await
        });

        // Send a frame with valid length but corrupt postcard payload.
        let bad_payload: Vec<u8> = vec![NATIVE_TCP_FRAMED_VERSION_V3, 0xFF, 0xFF, 0xFF];
        let len = u32::try_from(bad_payload.len()).unwrap();
        client_side.write_all(&len.to_be_bytes()).await.unwrap();
        client_side.write_all(&bad_payload).await.unwrap();
        client_side.flush().await.unwrap();
        let (s1, _, _, _) = read_response_frame(&mut client_side).await;
        assert_eq!(s1, WireStatus::ProtocolError);

        // Now a valid V3 request — still works.
        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        let meta = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(ns_proto(ns)),
            name: "after-bad".into(),
            data: Vec::new(),
        })
        .unwrap();
        let frame = encode_request_frame(99, "put_object", &meta, b"ok").unwrap();
        client_side.write_all(&frame).await.unwrap();
        client_side.flush().await.unwrap();
        let (s2, request_id, _, _) = read_response_frame(&mut client_side).await;
        assert_eq!(s2, WireStatus::Ok);
        assert_eq!(request_id, 99);

        drop(client_side);
        let _ = server_task.await;
    }

    /// Oversize length prefix → ProtocolError response + connection
    /// terminates (we can't trust the read stream after that — the
    /// declared length skewed it).
    #[tokio::test]
    async fn oversize_length_prefix_terminates_connection() {
        let server = make_server().await;
        let principal = TcpFramedPrincipal::new("", ConnectionId(1));
        let (mut client_side, mut server_side) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(&mut server_side, server, principal).await
        });

        // Length way above the cap.
        let evil_len = u32::MAX;
        client_side.write_all(&evil_len.to_be_bytes()).await.unwrap();
        client_side.flush().await.unwrap();
        // Server should reply with ProtocolError frame then drop.
        let (status, _, _, _) = read_response_frame(&mut client_side).await;
        assert_eq!(status, WireStatus::ProtocolError);

        let result = server_task.await.expect("task joined");
        assert!(
            matches!(
                result,
                Err(ConnectionError::WireDecode(WireDecodeError::Oversize { .. }))
            ),
            "expected oversize wire-decode error, got: {result:?}",
        );
    }
}
