//! Client-side TCP-framed [`FabricPeer`] implementation.
//!
//! `TcpFramedFabricPeer` holds one multiplexed TCP connection per
//! peer. Per-call `request_id` correlation lets many in-flight
//! fragments share the same connection — the same shape as the
//! gateway-side `TcpFramedClient` in `kiseki-client`, simplified
//! for the fabric (plaintext-only for now; TLS will mirror the
//! gateway pattern in a follow-up). Re-implemented locally instead
//! of pulled from `kiseki-client` because `kiseki-client` depends
//! on `kiseki-chunk-cluster`, not the other way around.
//!
//! Why this exists: the 2026-06-01 3-node loopback profile measured
//! `put_send.transport = 1 598 µs` per fragment under gRPC vs ~115 µs
//! of receiver-side work — the gRPC/h2 stack dominated. ADR-042 §2.2
//! moved the gateway↔client edge off gRPC for exactly this reason;
//! this client moves the fabric↔fabric edge too.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::native_contract::wire_tcp_framed::{
    build_request_header, decode_response_frame, WireDecodeError, WireStatus,
    NATIVE_TCP_FRAMED_MAX_BODY,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::peer::tcp_framed::wire::{
    DeleteFragmentMeta, DeleteFragmentResponse, EnvelopeMeta, GetFragmentMeta,
    GetFragmentResponseMeta, HasFragmentMeta, HasFragmentResponse, PutFragmentMeta,
    PutFragmentResponse, FABRIC_VERB_DELETE_FRAGMENT, FABRIC_VERB_GET_FRAGMENT,
    FABRIC_VERB_HAS_FRAGMENT, FABRIC_VERB_PUT_FRAGMENT,
};
use crate::peer::{FabricPeer, FabricPeerError};

/// Connect-deadline. Mirrors `kiseki-client::native::tcp_framed::client`
/// — peer unreachable inside this window fails the connect rather
/// than blocking forever (which silently consumed the pool's
/// connection budget pre-2026-04 GH #124).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pending-request map — writer inserts, reader resolves.
type PendingMap = DashMap<u64, oneshot::Sender<(WireStatus, Vec<u8>, Vec<u8>)>>;

/// Inner connection state — split out so the peer can lazy-connect
/// (matches the tonic `Channel` lazy-on-build pattern) and
/// reconnect on transport failures.
struct Inner {
    write_half: Box<dyn AsyncWrite + Send + Unpin>,
    pending: Arc<PendingMap>,
    next_request_id: AtomicU64,
    /// Reader-loop task. Aborted on drop / replacement.
    _reader_task: JoinHandle<()>,
}

/// Client-side connection. Cheap to clone via `Arc<TcpFramedFabricPeer>`.
///
/// Construction (`new_lazy`) does NOT connect — the connect happens on
/// first call (or after a transport error invalidated the prior
/// connection). This matches tonic's `Channel`, which is built lazy
/// and reconnects under the hood. It avoids the chicken-and-egg trap
/// the runtime hit on 2026-06-01 where node 1 waited 30s for nodes
/// 2/3 to be reachable while nodes 2/3 waited for node 1's listener
/// that hadn't started yet because node 1 was blocked in connect.
pub struct TcpFramedFabricPeer {
    name: String,
    addr: String,
    inner: AsyncMutex<Option<Inner>>,
}

impl TcpFramedFabricPeer {
    /// Build a peer without connecting. The first `call()` connects;
    /// subsequent calls reuse the connection until a transport error
    /// invalidates it, at which point the next call reconnects.
    /// Matches the tonic `Channel` lazy-on-build pattern so a peer
    /// not yet listening at startup doesn't block the cluster from
    /// coming up.
    #[must_use]
    pub fn new_lazy(name: String, addr: String) -> Arc<Self> {
        Arc::new(Self {
            name,
            addr,
            inner: AsyncMutex::new(None),
        })
    }

    /// Eager-connect form — preserved for tests and operators who
    /// want startup-time visibility into peer reachability.
    ///
    /// # Errors
    /// `io::Error` on TCP connect or the connect timeout.
    pub async fn connect_plaintext(name: String, addr: String) -> io::Result<Arc<Self>> {
        let peer = Self::new_lazy(name, addr);
        peer.ensure_connected().await?;
        Ok(peer)
    }

    /// Peer address (host:port). Used by the runtime to reconnect
    /// after a peer restart; also helps log/metric correlation.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Open a fresh TCP connection and spawn its reader loop.
    /// Caller holds the `inner` mutex while calling this and stores
    /// the result.
    async fn establish_connection(&self) -> io::Result<Inner> {
        let stream =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&self.addr))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "fabric TCP-framed connect timed out after {}s",
                            CONNECT_TIMEOUT.as_secs()
                        ),
                    )
                })??;
        // TCP_NODELAY — vectored writes (header + meta + bulk) hit a
        // Nagle 40 ms timer without it. Same gotcha as the gateway
        // client (`project_tcp_nodelay_pattern` memory).
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(error = %e, "fabric TCP-framed client: TCP_NODELAY failed; perf may regress");
        }
        let (read_half, write_half) = tokio::io::split(stream);
        let pending: Arc<PendingMap> = Arc::new(DashMap::new());
        let pending_for_reader = Arc::clone(&pending);
        let reader_task = tokio::spawn(reader_loop(read_half, pending_for_reader));
        Ok(Inner {
            write_half: Box::new(write_half),
            pending,
            next_request_id: AtomicU64::new(1),
            _reader_task: reader_task,
        })
    }

    /// Make sure the inner connection exists. Connects if absent,
    /// no-ops if present.
    async fn ensure_connected(&self) -> io::Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        *guard = Some(self.establish_connection().await?);
        Ok(())
    }

    /// Issue one fabric verb. Wraps the V3 wire encode, request_id
    /// correlation, and oneshot wait. Connects on first use and
    /// reconnects on transport error.
    async fn call(
        &self,
        verb_tag: &str,
        req_meta: Vec<u8>,
        req_bulk: Vec<u8>,
    ) -> Result<(WireStatus, Vec<u8>, Vec<u8>), FabricPeerError> {
        // Connect-if-needed under the inner mutex.
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            *guard =
                Some(self.establish_connection().await.map_err(|e| {
                    FabricPeerError::Unavailable(format!("fabric tcp connect: {e}"))
                })?);
        }
        let inner = guard.as_mut().expect("inner just set");
        let request_id = inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        inner.pending.insert(request_id, tx);

        let header = build_request_header(request_id, verb_tag, req_meta.len(), req_bulk.len())
            .map_err(|e| {
                inner.pending.remove(&request_id);
                FabricPeerError::Transport(format!("encode header: {e}"))
            })?;

        // Vectored write: [header, meta, bulk] in one syscall.
        if let Err(e) = inner.write_half.write_all(&header).await {
            inner.pending.remove(&request_id);
            // Transport error — invalidate the connection so the
            // next call reconnects.
            *guard = None;
            return Err(FabricPeerError::Transport(format!("write header: {e}")));
        }
        if !req_meta.is_empty() {
            if let Err(e) = inner.write_half.write_all(&req_meta).await {
                inner.pending.remove(&request_id);
                *guard = None;
                return Err(FabricPeerError::Transport(format!("write meta: {e}")));
            }
        }
        if !req_bulk.is_empty() {
            if let Err(e) = inner.write_half.write_all(&req_bulk).await {
                inner.pending.remove(&request_id);
                *guard = None;
                return Err(FabricPeerError::Transport(format!("write bulk: {e}")));
            }
        }
        // Release the inner lock before awaiting the response so
        // other concurrent calls can write their requests in
        // parallel (true multiplex on the same connection).
        drop(guard);

        rx.await
            .map_err(|_| FabricPeerError::Transport("connection closed before response".into()))
    }
}

/// Background reader: decodes one response frame at a time and
/// resolves the matching `request_id`'s oneshot. Exits on EOF /
/// IO error — outstanding pending entries are dropped (their
/// senders close, surfaced to the awaiter as "connection closed").
async fn reader_loop<R>(mut read_half: R, pending: Arc<PendingMap>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    loop {
        let mut len_buf = [0u8; 4];
        if read_half.read_exact(&mut len_buf).await.is_err() {
            // EOF or IO error — end the loop, drop pending.
            return;
        }
        let len_be = u32::from_be_bytes(len_buf);
        let body_len = match validate_body_len(len_be) {
            Ok(n) => n,
            Err(_) => return,
        };
        let mut body = vec![0u8; body_len];
        if read_half.read_exact(&mut body).await.is_err() {
            return;
        }
        let (status, request_id, meta, bulk) = match decode_response_frame(&body) {
            Ok(view) => (
                view.status,
                view.request_id,
                view.meta.to_vec(),
                view.bulk.to_vec(),
            ),
            Err(_) => continue, // malformed — drop, but stay connected
        };
        if let Some((_, sender)) = pending.remove(&request_id) {
            let _ = sender.send((status, meta, bulk));
        }
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

// -- FabricPeer impl -------------------------------------------------

#[async_trait]
impl FabricPeer for TcpFramedFabricPeer {
    fn name(&self) -> &str {
        &self.name
    }

    async fn put_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
        pool_id: String,
        envelope: Envelope,
    ) -> Result<bool, FabricPeerError> {
        let (env_meta, ciphertext) = EnvelopeMeta::split_from(envelope);
        let meta = PutFragmentMeta {
            chunk_id,
            fragment_index,
            tenant_id,
            pool_id,
            envelope: env_meta,
        };
        let meta_bytes = postcard::to_allocvec(&meta)
            .map_err(|e| FabricPeerError::Transport(format!("encode meta: {e}")))?;
        let (status, resp_meta, _bulk) = self
            .call(FABRIC_VERB_PUT_FRAGMENT, meta_bytes, ciphertext)
            .await?;
        wire_status_to_result(status, &resp_meta)?;
        let resp: PutFragmentResponse = postcard::from_bytes(&resp_meta).map_err(|e| {
            FabricPeerError::Transport(format!("decode put_fragment response: {e}"))
        })?;
        Ok(resp.stored)
    }

    async fn get_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<Envelope, FabricPeerError> {
        let meta = GetFragmentMeta {
            chunk_id,
            fragment_index,
        };
        let meta_bytes = postcard::to_allocvec(&meta)
            .map_err(|e| FabricPeerError::Transport(format!("encode meta: {e}")))?;
        let (status, resp_meta, resp_bulk) = self
            .call(FABRIC_VERB_GET_FRAGMENT, meta_bytes, Vec::new())
            .await?;
        wire_status_to_result(status, &resp_meta)?;
        let resp: GetFragmentResponseMeta = postcard::from_bytes(&resp_meta).map_err(|e| {
            FabricPeerError::Transport(format!("decode get_fragment response: {e}"))
        })?;
        Ok(resp.envelope.with_ciphertext(resp_bulk))
    }

    async fn delete_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
        tenant_id: OrgId,
    ) -> Result<bool, FabricPeerError> {
        let meta = DeleteFragmentMeta {
            chunk_id,
            fragment_index,
            tenant_id,
        };
        let meta_bytes = postcard::to_allocvec(&meta)
            .map_err(|e| FabricPeerError::Transport(format!("encode meta: {e}")))?;
        let (status, resp_meta, _bulk) = self
            .call(FABRIC_VERB_DELETE_FRAGMENT, meta_bytes, Vec::new())
            .await?;
        wire_status_to_result(status, &resp_meta)?;
        let resp: DeleteFragmentResponse = postcard::from_bytes(&resp_meta).map_err(|e| {
            FabricPeerError::Transport(format!("decode delete_fragment response: {e}"))
        })?;
        Ok(resp.deleted)
    }

    async fn has_fragment(
        &self,
        chunk_id: ChunkId,
        fragment_index: u32,
    ) -> Result<bool, FabricPeerError> {
        let meta = HasFragmentMeta {
            chunk_id,
            fragment_index,
        };
        let meta_bytes = postcard::to_allocvec(&meta)
            .map_err(|e| FabricPeerError::Transport(format!("encode meta: {e}")))?;
        let (status, resp_meta, _bulk) = self
            .call(FABRIC_VERB_HAS_FRAGMENT, meta_bytes, Vec::new())
            .await?;
        wire_status_to_result(status, &resp_meta)?;
        let resp: HasFragmentResponse = postcard::from_bytes(&resp_meta).map_err(|e| {
            FabricPeerError::Transport(format!("decode has_fragment response: {e}"))
        })?;
        Ok(resp.present)
    }
}

/// Translate the wire status byte into a [`FabricPeerError`]. The
/// status mapping mirrors the gRPC variant so callers don't have to
/// know which transport is underneath. On non-Ok statuses, the
/// response `meta` carries a UTF-8 reason string.
fn wire_status_to_result(status: WireStatus, reason_bytes: &[u8]) -> Result<(), FabricPeerError> {
    if status == WireStatus::Ok {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(reason_bytes).into_owned();
    Err(match status {
        WireStatus::NotFound => FabricPeerError::NotFound,
        WireStatus::Unavailable => FabricPeerError::Unavailable(reason),
        WireStatus::PermissionDenied | WireStatus::Unauthenticated => {
            FabricPeerError::Rejected(reason)
        }
        _ => FabricPeerError::Transport(format!("{status:?}: {reason}")),
    })
}
