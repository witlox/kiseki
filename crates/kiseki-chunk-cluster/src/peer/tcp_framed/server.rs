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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;

use crate::peer::tcp_framed::wire::{
    DeleteFragmentMeta, DeleteFragmentResponse, EnvelopeMeta, GetFragmentMeta,
    GetFragmentResponseMeta, HasFragmentMeta, HasFragmentResponse, PutFragmentMeta,
    PutFragmentResponse, FABRIC_VERB_DELETE_FRAGMENT, FABRIC_VERB_GET_FRAGMENT,
    FABRIC_VERB_HAS_FRAGMENT, FABRIC_VERB_PUT_FRAGMENT,
};
use crate::server::ClusterChunkServer;

/// Listener for the TCP-framed fabric. Cheap to clone via `Arc`; the
/// runtime wires one per node.
pub struct TcpFramedFabricListener {
    /// Bind address — `0.0.0.0:port` or similar.
    addr: std::net::SocketAddr,
    /// Shared handler — the same struct the gRPC adapter wraps.
    handler: Arc<ClusterChunkServer>,
}

impl TcpFramedFabricListener {
    /// Build a listener that will spawn one task per accepted
    /// connection and dispatch through `handler`.
    #[must_use]
    pub fn new(addr: std::net::SocketAddr, handler: Arc<ClusterChunkServer>) -> Self {
        Self { addr, handler }
    }

    /// Bind + run the accept loop. Returns only on `listener.accept()`
    /// failure (treated as fatal — runtime should restart the listener).
    ///
    /// # Errors
    /// `io::Error` from `bind` or `accept`.
    pub async fn run(self) -> io::Result<()> {
        let listener = TcpListener::bind(self.addr).await?;
        tracing::warn!(
            target: "kiseki::fabric::tcp_framed",
            addr = %self.addr,
            "fabric TCP-framed listener up (plaintext — dev mode; TLS variant TBD)",
        );
        loop {
            let (stream, peer) = listener.accept().await?;
            // TCP_NODELAY — same Nagle 40ms trap as the gateway and the
            // client side of this binding.
            if let Err(e) = stream.set_nodelay(true) {
                tracing::warn!(error = %e, %peer, "fabric TCP-framed: TCP_NODELAY failed");
            }
            let handler = Arc::clone(&self.handler);
            tokio::spawn(async move {
                if let Err(e) = serve_connection(stream, handler).await {
                    tracing::debug!(%peer, error = %e, "fabric TCP-framed: connection ended");
                }
            });
        }
    }
}

/// Per-connection read/dispatch/write loop. Errors here close the
/// connection; the client reconnects.
async fn serve_connection(stream: TcpStream, handler: Arc<ClusterChunkServer>) -> io::Result<()> {
    let (mut read_half, write_half) = stream.into_split();
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
            if !resp_bulk.is_empty() && guard.write_all(&resp_bulk).await.is_err() {
                return;
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

async fn handle_put_fragment(
    handler: &ClusterChunkServer,
    view: RequestFrameView<'_>,
) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let decode_started = std::time::Instant::now();
    let meta: PutFragmentMeta = match postcard::from_bytes(view.meta) {
        Ok(m) => m,
        Err(e) => return err_outcome(WireStatus::InvalidArgument, &format!("bad meta: {e}")),
    };
    let envelope = meta.envelope.with_ciphertext(view.bulk.to_vec());

    // Mirror the gRPC method: record envelope crypto, observe decode
    // phase, route by fragment_index.
    let chunk_id = envelope.chunk_id;
    handler.chunk_envelope_meta_for_tcp_framed().record(
        chunk_id,
        envelope.auth_tag,
        envelope.nonce,
        envelope.system_epoch,
        envelope.tenant_epoch,
        envelope.tenant_wrapped_material.clone(),
    );
    if let Some(m) = handler.metrics_for_tcp_framed() {
        m.observe_put_recv("decode", decode_started.elapsed());
    }

    let pool = if meta.pool_id.is_empty() {
        handler.default_pool_for_tcp_framed().to_owned()
    } else {
        meta.pool_id
    };

    let write_started = std::time::Instant::now();
    let stored_result = if meta.fragment_index == 0 {
        handler
            .local_for_tcp_framed()
            .write_chunk(envelope, &pool)
            .await
    } else {
        match handler
            .local_for_tcp_framed()
            .write_fragment_in_pool(&chunk_id, meta.fragment_index, envelope.ciphertext, &pool)
            .await
        {
            Ok(()) => Ok(true), // EC fragment writes report stored=true
            Err(e) => Err(e),
        }
    };
    if let Some(m) = handler.metrics_for_tcp_framed() {
        m.observe_put_recv("write_chunk", write_started.elapsed());
    }
    match stored_result {
        Ok(stored) => {
            let resp = PutFragmentResponse { stored };
            let meta_bytes = postcard::to_allocvec(&resp).unwrap_or_default();
            (WireStatus::Ok, meta_bytes, Vec::new())
        }
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

    if meta.fragment_index == 0 {
        // Same dual path as gRPC: try fragment then whole-envelope.
        if let Ok(bytes) = handler
            .local_for_tcp_framed()
            .read_fragment(&meta.chunk_id, 0)
            .await
        {
            let env = handler.envelope_from_bytes_for_tcp_framed(meta.chunk_id, bytes);
            let (em, ct) = EnvelopeMeta::split_from(env);
            let resp = GetFragmentResponseMeta { envelope: em };
            let meta_bytes = postcard::to_allocvec(&resp).unwrap_or_default();
            return (WireStatus::Ok, meta_bytes, ct);
        }
        match handler
            .local_for_tcp_framed()
            .read_chunk(&meta.chunk_id, None)
            .await
        {
            Ok(env) => {
                let (em, ct) = EnvelopeMeta::split_from(env);
                let resp = GetFragmentResponseMeta { envelope: em };
                let meta_bytes = postcard::to_allocvec(&resp).unwrap_or_default();
                (WireStatus::Ok, meta_bytes, ct)
            }
            Err(e) => err_from_chunk(&e),
        }
    } else {
        match handler
            .local_for_tcp_framed()
            .read_fragment(&meta.chunk_id, meta.fragment_index)
            .await
        {
            Ok(bytes) => {
                let env = handler.envelope_from_bytes_for_tcp_framed(meta.chunk_id, bytes);
                let (em, ct) = EnvelopeMeta::split_from(env);
                let resp = GetFragmentResponseMeta { envelope: em };
                let meta_bytes = postcard::to_allocvec(&resp).unwrap_or_default();
                (WireStatus::Ok, meta_bytes, ct)
            }
            Err(e) => err_from_chunk(&e),
        }
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
    if meta.fragment_index == 0 {
        match handler
            .local_for_tcp_framed()
            .decrement_refcount(&meta.chunk_id)
            .await
        {
            Ok(0) => ok_with(&DeleteFragmentResponse { deleted: true }),
            Ok(_) => ok_with(&DeleteFragmentResponse { deleted: false }),
            Err(ChunkError::NotFound(_)) => ok_with(&DeleteFragmentResponse { deleted: false }),
            Err(e) => err_from_chunk(&e),
        }
    } else {
        match handler
            .local_for_tcp_framed()
            .delete_fragment(&meta.chunk_id, meta.fragment_index)
            .await
        {
            Ok(was_present) => ok_with(&DeleteFragmentResponse {
                deleted: was_present,
            }),
            Err(e) => err_from_chunk(&e),
        }
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
    let present = if meta.fragment_index == 0 {
        match handler
            .local_for_tcp_framed()
            .refcount(&meta.chunk_id)
            .await
        {
            Ok(rc) => rc > 0,
            Err(ChunkError::NotFound(_)) => false,
            Err(e) => return err_from_chunk(&e),
        }
    } else {
        handler
            .local_for_tcp_framed()
            .list_fragments(&meta.chunk_id)
            .await
            .contains(&meta.fragment_index)
    };
    ok_with(&HasFragmentResponse { present })
}

fn ok_with<T: serde::Serialize>(resp: &T) -> (WireStatus, Vec<u8>, Vec<u8>) {
    let meta_bytes = postcard::to_allocvec(resp).unwrap_or_default();
    (WireStatus::Ok, meta_bytes, Vec::new())
}
