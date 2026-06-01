//! Server-side TCP-framed fabric listener — accept connections, demux
//! frames by `request_id`, dispatch the four fabric verbs to the
//! shared `ClusterChunkServer` handler logic.
//!
//! The dispatch body mirrors the bodies of
//! [`crate::server::ClusterChunkServer`]'s gRPC method impls one-for-
//! one (same `local` ops calls, same envelope-meta recording, same
//! metric labels) so a fragment landing via TCP-framed is
//! indistinguishable from one landing via gRPC at the storage layer.
//! Duplicated rather than refactored into a shared inherent method —
//! deliberate first-cut; refactor lifted to a follow-up if/when the
//! gRPC adapter goes away entirely.

use std::io;
use std::sync::Arc;

use kiseki_chunk::error::ChunkError;
use kiseki_proto::native_contract::wire_tcp_framed::{
    build_response_header, decode_request_frame, RequestFrameView, WireDecodeError, WireStatus,
    NATIVE_TCP_FRAMED_MAX_BODY,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_rustls::TlsAcceptor;

use crate::peer::tcp_framed::wire::{
    DeleteFragmentMeta, DeleteFragmentResponse, EnvelopeMeta, GetFragmentMeta,
    GetFragmentResponseMeta, HasFragmentMeta, HasFragmentResponse, PutFragmentMeta,
    PutFragmentResponse, FABRIC_VERB_DELETE_FRAGMENT, FABRIC_VERB_GET_FRAGMENT,
    FABRIC_VERB_HAS_FRAGMENT, FABRIC_VERB_PUT_FRAGMENT,
};
use crate::server::ClusterChunkServer;

/// Listener for the TCP-framed fabric. Cheap to clone via `Arc`; the
/// runtime wires one per node.
///
/// Transport posture:
///   - `tls_acceptor: Some(_)` → mTLS at handshake; plaintext sockets
///     are rejected by the rustls handshake.
///   - `tls_acceptor: None`    → plaintext (dev mode). The runtime
///     emits the standard `PLAINTEXT — development only` warning at
///     wire time, the same posture the gateway TCP-framed listener
///     and the gRPC fabric service take when `cfg.tls` is unset.
pub struct TcpFramedFabricListener {
    /// Bind address — `0.0.0.0:port` or similar.
    addr: std::net::SocketAddr,
    /// Shared handler — the same struct the gRPC adapter wraps.
    handler: Arc<ClusterChunkServer>,
    /// rustls acceptor — `None` for plaintext dev mode. mTLS uses the
    /// same cluster CA the gRPC fabric service trusts.
    tls_acceptor: Option<TlsAcceptor>,
}

impl TcpFramedFabricListener {
    /// Build a plaintext listener — dev only. Production callers use
    /// [`Self::with_tls`] to enforce the cluster mTLS posture.
    #[must_use]
    pub fn new(addr: std::net::SocketAddr, handler: Arc<ClusterChunkServer>) -> Self {
        Self {
            addr,
            handler,
            tls_acceptor: None,
        }
    }

    /// Build an mTLS-enforcing listener. Per-connection rustls
    /// handshake runs before the first frame is read; a peer
    /// presenting no client cert is rejected by the rustls policy
    /// (same posture as the gRPC fabric service).
    #[must_use]
    pub fn with_tls(
        addr: std::net::SocketAddr,
        handler: Arc<ClusterChunkServer>,
        server_config: Arc<rustls::ServerConfig>,
    ) -> Self {
        Self {
            addr,
            handler,
            tls_acceptor: Some(TlsAcceptor::from(server_config)),
        }
    }

    /// Bind + run the accept loop. Returns only on `listener.accept()`
    /// failure (treated as fatal — runtime should restart the listener).
    ///
    /// # Errors
    /// `io::Error` from `bind` or `accept`.
    pub async fn run(self) -> io::Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        if self.tls_acceptor.is_some() {
            tracing::info!(
                target: "kiseki::fabric::tcp_framed",
                addr = %self.addr,
                "fabric TCP-framed listener up (mTLS)",
            );
        } else {
            tracing::warn!(
                target: "kiseki::fabric::tcp_framed",
                addr = %self.addr,
                "fabric TCP-framed listener up (PLAINTEXT — development only)",
            );
        }
        loop {
            let (stream, peer) = listener.accept().await?;
            // TCP_NODELAY — same Nagle 40ms trap as the gateway and the
            // client side of this binding.
            if let Err(e) = stream.set_nodelay(true) {
                tracing::warn!(error = %e, %peer, "fabric TCP-framed: TCP_NODELAY failed");
            }
            let handler = Arc::clone(&self.handler);
            let acceptor = self.tls_acceptor.clone();
            tokio::spawn(async move {
                let res = match acceptor {
                    Some(acc) => match acc.accept(stream).await {
                        Ok(tls) => serve_connection(tls, handler).await,
                        Err(e) => {
                            tracing::debug!(%peer, error = %e, "fabric TCP-framed: TLS handshake failed");
                            Ok(())
                        }
                    },
                    None => serve_connection(stream, handler).await,
                };
                if let Err(e) = res {
                    tracing::debug!(%peer, error = %e, "fabric TCP-framed: connection ended");
                }
            });
        }
    }
}

/// Per-connection read/dispatch/write loop. Errors here close the
/// connection; the client reconnects. Generic over `AsyncRead +
/// AsyncWrite` so the same loop drives plaintext `TcpStream` and
/// the rustls-wrapped variant.
async fn serve_connection<S>(stream: S, handler: Arc<ClusterChunkServer>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut read_half, write_half) = tokio::io::split(stream);
    let write_half = Arc::new(AsyncMutex::new(write_half));
    loop {
        // Read frame length.
        let mut len_buf = [0u8; 4];
        if let Err(e) = read_half.read_exact(&mut len_buf).await {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Ok(()); // peer closed
            }
            return Err(e);
        }
        let body_len = match validate_body_len(u32::from_be_bytes(len_buf)) {
            Ok(n) => n,
            Err(_) => return Ok(()), // oversize / malformed — close
        };
        let mut body = vec![0u8; body_len];
        read_half.read_exact(&mut body).await?;

        // Move body into the per-frame dispatch task so concurrent
        // requests on the same connection run in parallel.
        let handler = Arc::clone(&handler);
        let write_half = Arc::clone(&write_half);
        tokio::spawn(async move {
            let view = match decode_request_frame(&body) {
                Ok(v) => v,
                Err(_) => return,
            };
            let request_id = view.request_id;
            let (status, resp_meta, resp_bulk) = dispatch(&handler, view).await;
            // `build_response_header` already prepends the V3 length
            // prefix — write [header, meta, bulk] as three buffers
            // exactly like the gateway-side `write_response` (no
            // extra length prefix here; that was the 2026-06-01
            // double-length bug).
            let header =
                match build_response_header(status, request_id, resp_meta.len(), resp_bulk.len()) {
                    Ok(h) => h,
                    Err(_) => return,
                };
            let mut guard = write_half.lock().await;
            if guard.write_all(&header).await.is_err() {
                return;
            }
            if !resp_meta.is_empty() && guard.write_all(&resp_meta).await.is_err() {
                return;
            }
            if !resp_bulk.is_empty() {
                let _ = guard.write_all(&resp_bulk).await;
            }
        });
    }
}

fn validate_body_len(len_be: u32) -> Result<usize, WireDecodeError> {
    let n = usize::try_from(len_be).map_err(|_| WireDecodeError::Oversize {
        len: len_be as usize,
        cap: NATIVE_TCP_FRAMED_MAX_BODY,
    })?;
    if n > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireDecodeError::Oversize {
            len: n,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    Ok(n)
}

/// Map the verb to a handler and return `(status, meta_bytes, bulk_bytes)`.
async fn dispatch(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    // RequestFrameView already gives us verb_tag as &str (the wire
    // decoder validated UTF-8 on the way in).
    match view.verb_tag {
        FABRIC_VERB_PUT_FRAGMENT => handle_put_fragment(handler, view).await,
        FABRIC_VERB_GET_FRAGMENT => handle_get_fragment(handler, view).await,
        FABRIC_VERB_DELETE_FRAGMENT => handle_delete_fragment(handler, view).await,
        FABRIC_VERB_HAS_FRAGMENT => handle_has_fragment(handler, view).await,
        other => err_outcome(
            WireStatus::UnknownVerb,
            &format!("unknown fabric verb: {other}"),
        ),
    }
}

fn err_outcome(status: WireStatus, reason: &str) -> (WireStatus, Vec<u8>, Vec<u8>) {
    (status, reason.as_bytes().to_vec(), Vec::new())
}

fn err_from_chunk(e: &ChunkError) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let (status, msg) = match e {
        ChunkError::NotFound(_) => (WireStatus::NotFound, e.to_string()),
        ChunkError::Io(s) => (WireStatus::Unavailable, s.clone()),
        ChunkError::PoolFull(_) => (WireStatus::ResourceExhausted, e.to_string()),
        _ => (WireStatus::Internal, e.to_string()),
    };
    (status, msg.into_bytes(), Vec::new())
}

// -- put_fragment ----------------------------------------------------
// All four verb handlers now delegate to shared `handle_*` methods on
// `ClusterChunkServer`. The adapter's job here is purely
// codec + status mapping: postcard-decode the typed meta + bulk,
// call the shared handler, postcard-encode the response.

async fn handle_put_fragment(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta: PutFragmentMeta = match postcard::from_bytes(view.meta) {
        Ok(m) => m,
        Err(e) => return err_outcome(WireStatus::InvalidArgument, &format!("bad meta: {e}")),
    };
    let envelope = meta.envelope.with_ciphertext(view.bulk.to_vec());
    let pool = if meta.pool_id.is_empty() {
        handler.default_pool_for_tcp_framed().to_owned()
    } else {
        meta.pool_id
    };
    match handler
        .handle_put_fragment(meta.fragment_index, pool, envelope)
        .await
    {
        Ok(stored) => ok_with(&PutFragmentResponse { stored }),
        Err(e) => err_from_chunk(&e),
    }
}

// -- get_fragment ----------------------------------------------------

async fn handle_get_fragment(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta: GetFragmentMeta = match postcard::from_bytes(view.meta) {
        Ok(m) => m,
        Err(e) => return err_outcome(WireStatus::InvalidArgument, &format!("bad meta: {e}")),
    };
    match handler
        .handle_get_fragment(meta.chunk_id, meta.fragment_index)
        .await
    {
        Ok(env) => {
            // Split envelope onto meta + bulk lanes — the V3 perf trick.
            let (em, ct) = EnvelopeMeta::split_from(env);
            let resp = GetFragmentResponseMeta { envelope: em };
            let meta_bytes = postcard::to_allocvec(&resp).unwrap_or_default();
            (WireStatus::Ok, meta_bytes, ct)
        }
        Err(e) => err_from_chunk(&e),
    }
}

// -- delete_fragment -------------------------------------------------

async fn handle_delete_fragment(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta: DeleteFragmentMeta = match postcard::from_bytes(view.meta) {
        Ok(m) => m,
        Err(e) => return err_outcome(WireStatus::InvalidArgument, &format!("bad meta: {e}")),
    };
    match handler
        .handle_delete_fragment(meta.chunk_id, meta.fragment_index)
        .await
    {
        Ok(deleted) => ok_with(&DeleteFragmentResponse { deleted }),
        Err(e) => err_from_chunk(&e),
    }
}

// -- has_fragment ----------------------------------------------------

async fn handle_has_fragment(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta: HasFragmentMeta = match postcard::from_bytes(view.meta) {
        Ok(m) => m,
        Err(e) => return err_outcome(WireStatus::InvalidArgument, &format!("bad meta: {e}")),
    };
    match handler
        .handle_has_fragment(meta.chunk_id, meta.fragment_index)
        .await
    {
        Ok(present) => ok_with(&HasFragmentResponse { present }),
        Err(e) => err_from_chunk(&e),
    }
}

fn ok_with<T: serde::Serialize>(resp: &T) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta_bytes = postcard::to_allocvec(resp).unwrap_or_default();
    (WireStatus::Ok, meta_bytes, Vec::new())
}
