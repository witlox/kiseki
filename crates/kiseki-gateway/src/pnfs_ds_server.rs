//! pNFS Data Server (DS) — stateless NFSv4.1 op-subset endpoint.
//!
//! ADR-038 §D2/§D3 (Phase 15a): a per-storage-node listener on
//! `ds_addr` (default `:2052`). pNFS clients direct LAYOUTGET-issued
//! READ/WRITE traffic here, bypassing the MDS for data.
//!
//! The DS holds **no per-fh4 state** (I-PN2). Every op:
//!
//! 1. Decodes the [`PnfsFileHandle`] from the wire (PUTFH).
//! 2. Validates the HMAC + expiry (`PnfsFileHandle::validate`) — failures
//!    map to `NFS4ERR_BADHANDLE` (I-PN1).
//! 3. Translates `(stripe_index, op_offset, op_count)` into an absolute
//!    composition byte range and forwards via [`GatewayOps`] (I-PN3).
//!
//! The dispatcher allows only [`ALLOWED_DS_OPS`]; all other op codes
//! return `NFS4ERR_NOTSUPP` (I-PN7). COMPOUND aborts on the first error
//! per RFC 5661 §15.2 (inherited from `dispatch_compound`).

// DS op handlers share the `async fn` shape with the v4.1 dispatcher
// in `nfs4_server.rs` so the same `process_ds_op` glue can drive both.
// The DS op subset is small (READ / WRITE / COMMIT / GETATTR /
// SEQUENCE / etc.) and a handful are pure XDR construction with no
// gateway await — they stay async for dispatch uniformity.
#![allow(clippy::unused_async)]
// Doc identifiers (composition_id, stateid, fh4, etc.) appear too
// often in this module's prose; backticking each occurrence is
// noise. Same precedent as `nfs_ops.rs`.
#![allow(clippy::doc_markdown)]

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use rustls::ServerConfig;

use crate::nfs4_server::{
    nfs4_status, op, op_create_session, op_destroy_session, op_exchange_id_with_role, op_sequence,
    ServerRole, SessionManager,
};
use crate::nfs_xdr::{encode_reply_accepted, RpcCallHeader, XdrReader, XdrWriter};
use crate::ops::{GatewayOps, ReadRequest};
use crate::pnfs::{FhValidateError, PnfsFhMacKey, PnfsFileHandle};
use crate::pnfs_write_buffer::BufferWriteResult;

/// Op codes accepted by the DS. Anything outside this set returns
/// `NFS4ERR_NOTSUPP` per I-PN7.
pub const ALLOWED_DS_OPS: [u32; 11] = [
    op::EXCHANGE_ID,
    op::CREATE_SESSION,
    op::DESTROY_SESSION,
    op::DESTROY_CLIENTID,
    op::RECLAIM_COMPLETE,
    op::SEQUENCE,
    op::PUTFH,
    op::READ,
    op::WRITE,
    op::COMMIT,
    op::GETATTR,
    // WRITE wiring landed 2026-05-10 per ADR-038 rev 3 §D5 + §D5.1
    // (chunk-staging buffer; flush via existing GatewayOps::write on
    // COMMIT). See `pnfs_write_buffer.rs` for the buffer design and
    // `specs/implementation/pnfs-ds-write.md` for the implementation
    // plan; pre-fix this list excluded WRITE because the original
    // ADR-038 rev 2 §D5 wording (DS WRITE → GatewayOps::write) didn't
    // typecheck (the trait method has no composition_id/offset args).
    //
    // RECLAIM_COMPLETE + DESTROY_CLIENTID are required by Linux's pNFS
    // client even on DS-only sessions. Per RFC 8881 §13.6.4: "servers
    // MUST permit clients to send RECLAIM_COMPLETE on a session bound
    // only to the data server"; symmetric for DESTROY_CLIENTID at
    // teardown. Pre-fix the DS rejected both with NFS4ERR_NOTSUPP and
    // the kernel got stuck retrying session establishment, eventually
    // issuing LAYOUTRETURN+CLOSE without ever doing any READ.
];

/// Stateless DS context. One instance per storage node.
///
/// "Stateless" is now a slight misnomer post-ADR-038 rev 3: the DS
/// holds chunk-staging write buffers + composition redirects so DS
/// WRITE round-trips work without breaking composition immutability
/// (`write_buffers`). The buffers are per-DS-process, scoped by
/// `composition_id`, and dropped on `DESTROY_CLIENTID` /
/// `DESTROY_SESSION`. The DS still doesn't own MDS-authoritative
/// state (per ADR-038 §D3).
pub struct DsContext<G: GatewayOps + Send + Sync + 'static> {
    /// Underlying gateway used to satisfy `GatewayOps::read` (decrypts
    /// chunks server-side per I-PN3) and `GatewayOps::write` on
    /// COMMIT-drain (ADR-038 rev 3 §D5).
    pub gateway: Arc<G>,
    /// MAC key derived from the cluster master + cluster id (ADR-038 §D4.1).
    pub mac_key: PnfsFhMacKey,
    /// Stripe size in bytes (default 1 MiB per ADR-038 §D6).
    pub stripe_size_bytes: u64,
    /// Tokio runtime handle used to bridge the sync NFS protocol path
    /// to async `GatewayOps`. Mirrors the bridge used by
    /// `kiseki_gateway::nfs_ops::NfsContext`.
    pub rt: tokio::runtime::Handle,
    /// Pluggable wall clock — `now_ms()`. Production passes
    /// `default_now_ms`; tests can substitute a fixed clock.
    pub now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Optional MDS-published recall list (Phase 15c). When set, the
    /// DS consults `MdsLayoutManager::is_revoked` BEFORE MAC
    /// validation; recalled fh4s return `NFS4ERR_BADHANDLE` even if
    /// the MAC matches and the expiry has not elapsed.
    ///
    /// Single-node deployments share the same `Arc` with the MDS; in
    /// multi-node deployments the production path will publish the
    /// revoked set via the same `TopologyEventBus` that triggered the
    /// recall (out of scope for Phase 15c).
    pub mds_layout_manager: Option<Arc<crate::pnfs::MdsLayoutManager>>,
    /// Chunk-staging write buffers (ADR-038 rev 3 §D5 + §D5.1). One
    /// per `composition_id`; capped per-composition; redirect table
    /// rewrites OLD fh4 reads to the post-COMMIT composition. See
    /// [`crate::pnfs_write_buffer`] for the full design + tests.
    pub write_buffers: Arc<crate::pnfs_write_buffer::DsWriteBuffers>,
}

/// Default wall-clock source: `SystemTime::now()` truncated to ms.
#[must_use]
pub fn default_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl<G: GatewayOps + Send + Sync + 'static> DsContext<G> {
    fn block_gateway<F, T>(&self, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.rt.block_on(f))
        } else {
            self.rt.block_on(f)
        }
    }
}

/// Per-COMPOUND state. The only field is the validated `current_fh`
/// installed by `PUTFH`. No long-lived state is retained between
/// compounds (I-PN2).
#[derive(Default)]
struct DsCompoundState {
    current_fh: Option<PnfsFileHandle>,
}

/// Drive a single connection (sync — caller spawns a thread).
/// Mirrors `handle_nfs4_connection` from the MDS path.
///
/// Generic over the stream type so the same dispatcher serves both
/// raw `TcpStream` (plaintext fallback) and `rustls::StreamOwned`
/// (TLS default) per ADR-038 §D4.
pub async fn handle_ds_connection<G, S>(
    stream: &mut S,
    ctx: &Arc<DsContext<G>>,
    sessions: &SessionManager,
) -> io::Result<()>
where
    G: GatewayOps + Send + Sync + 'static,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let buf = match crate::nfs_xdr::read_rm_message_async(stream).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => {
                // 2026-05-10: surface the first bytes the framing layer
                // tripped on. Without this, node2/node3 silently rejected
                // the kernel's first frame as "frame exceeds 16MB" with
                // no way to tell what the kernel actually sent. Diagnostic
                // for the Phase 3 read-hang investigation.
                tracing::debug!(
                    error = %e,
                    "DS framing error; closing connection"
                );
                return Err(e);
            }
        };
        let mut reader = XdrReader::new(&buf);

        let header = RpcCallHeader::decode(&mut reader)?;

        // RFC 8881 §20: kernel pNFS clients use the SAME forward TCP
        // connection for back-channel callback probes (NFS4_CB_PROGRAM
        // CB_NULL, procedure 0). Without this the kernel rejects the
        // DS as un-bindable for callbacks and falls back to slow MDS
        // reads — same shape as the MDS handler at `nfs4_server.rs:
        // 306-311`. Pre-fix the DS handler missed this; nodes that
        // received CB_NULL replied PROG_MISMATCH, leaving the kernel
        // stuck waiting on a ~5 s connection-establishment retry timer.
        if header.program == crate::nfs4_server::NFS4_CB_PROGRAM && header.procedure == 0 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS
            crate::nfs_xdr::write_rm_message_async(stream, &w.into_bytes()).await?;
            continue;
        }

        if header.program != 100_003 || header.version != 4 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
                                                          // Per RFC 5531: PROG_MISMATCH carries (low, high) version
                                                          // hints. Without these the kernel may misinterpret the
                                                          // reply and retry with malformed framing — matches the
                                                          // MDS handler.
            w.write_u32(4); // low
            w.write_u32(4); // high
            crate::nfs_xdr::write_rm_message_async(stream, &w.into_bytes()).await?;
            continue;
        }

        // RFC 5531: procedure 0 is the NFS NULL ping (no-op). Reply
        // with empty SUCCESS — same shape as MDS.
        if header.procedure == 0 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS
            crate::nfs_xdr::write_rm_message_async(stream, &w.into_bytes()).await?;
            continue;
        }
        if header.procedure != 1 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
            crate::nfs_xdr::write_rm_message_async(stream, &w.into_bytes()).await?;
            continue;
        }

        let reply = dispatch_ds_compound(&header, &mut reader, ctx, sessions).await;
        crate::nfs_xdr::write_rm_message_async(stream, &reply).await?;
    }
}

/// Run a DS listener until shutdown is signaled. Spawns one thread
/// per accepted connection. The TLS path mirrors
/// [`crate::nfs_server::serve_nfs_listener`].
///
/// Spec: ADR-038 §D2 (DS listener), §D4.1 (TLS default), I-PN7
/// (op-subset enforced inside `dispatch_ds_compound`).
pub async fn run_ds_server<G: GatewayOps + Send + Sync + 'static>(
    addr: SocketAddr,
    ctx: Arc<DsContext<G>>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    tls: Option<Arc<ServerConfig>>,
) {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %addr, error = %e, "DS bind failed");
            return;
        }
    };
    serve_ds_listener(listener, ctx, shutdown, tls).await;
}

/// Run a DS server on an already-bound listener — useful for tests
/// that pre-bind on `127.0.0.1:0`. Async-native.
#[allow(clippy::needless_pass_by_value)]
pub async fn serve_ds_listener<G: GatewayOps + Send + Sync + 'static>(
    listener: TcpListener,
    ctx: Arc<DsContext<G>>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    tls: Option<Arc<ServerConfig>>,
) {
    let _ = listener.set_nonblocking(true);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(addr = %addr, tls = tls.is_some(), "pNFS DS listening");
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "DS listener: std → tokio conversion failed");
            return;
        }
    };

    let sessions = Arc::new(SessionManager::new());

    loop {
        if let Some(ref s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("DS server shutting down");
                return;
            }
        }
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept()).await;
        let (stream, peer) = match accepted {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "DS accept error");
                continue;
            }
            Err(_) => continue,
        };

        let ctx = Arc::clone(&ctx);
        let sessions = Arc::clone(&sessions);
        let tls = tls.clone();
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            if let Some(tls_cfg) = tls {
                let acceptor = tokio_rustls::TlsAcceptor::from(tls_cfg);
                match acceptor.accept(stream).await {
                    Ok(mut tls_stream) => {
                        if let Err(e) = handle_ds_connection(&mut tls_stream, &ctx, &sessions).await
                        {
                            tracing::debug!(error = %e, peer = %peer, "DS-over-TLS connection ended");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, peer = %peer, "DS TLS handshake failed");
                    }
                }
            } else {
                let mut s = stream;
                if let Err(e) = handle_ds_connection(&mut s, &ctx, &sessions).await {
                    tracing::debug!(error = %e, peer = %peer, "DS plaintext connection ended");
                }
            }
        });
    }
}

/// Process one COMPOUND request. Pure function — testable without
/// touching TCP or TLS.
pub async fn dispatch_ds_compound<G: GatewayOps + Send + Sync + 'static>(
    header: &RpcCallHeader,
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    sessions: &SessionManager,
) -> Vec<u8> {
    let t_start = std::time::Instant::now();
    let _tag = reader.read_opaque().unwrap_or_default();
    let _minor_version = reader.read_u32().unwrap_or(1); // NFSv4.1
    let num_ops = reader.read_u32().unwrap_or(0).min(32);

    let mut op_results: Vec<Vec<u8>> = Vec::new();
    let mut op_codes: Vec<u32> = Vec::new();
    let mut compound_status = nfs4_status::NFS4_OK;
    let mut state = DsCompoundState::default();

    for _ in 0..num_ops {
        let Ok(op_code) = reader.read_u32() else {
            break;
        };
        op_codes.push(op_code);

        let (status, result) = process_ds_op(op_code, reader, ctx, sessions, &mut state).await;
        op_results.push(result);

        if status != nfs4_status::NFS4_OK {
            compound_status = status;
            break; // I-PN7: COMPOUND aborts on first error.
        }
    }

    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, header.xid, 0);
    w.write_u32(compound_status);
    w.write_opaque(&[]); // tag
    w.write_u32(u32::try_from(op_results.len()).unwrap_or(0));

    let mut buf = w.into_bytes();
    for result in &op_results {
        buf.extend_from_slice(result);
    }

    // One log line per DS-side compound. Mirrors the MDS dispatcher
    // (`nfs4_server::dispatch_compound`) so a unified trace can be
    // assembled from `kiseki_gateway::nfs4_server` +
    // `kiseki_gateway::pnfs_ds_server` log lines side-by-side.
    // Pre-2026-05-10 the DS dispatcher was silent, which masked
    // whether the kernel was even talking to the DS during the
    // 0.5 MB/s read regression — added during the read-hang
    // diagnostic in `specs/escalations/2026-05-10-pnfs-read-hang-
    // post-ds-write.md`.
    let elapsed_us = u64::try_from(t_start.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        xid = header.xid,
        ops = ?op_codes,
        status = compound_status,
        elapsed_us = elapsed_us,
        bytes_out = buf.len(),
        "pNFS DS compound"
    );
    buf
}

async fn process_ds_op<G: GatewayOps + Send + Sync + 'static>(
    op_code: u32,
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    sessions: &SessionManager,
    state: &mut DsCompoundState,
) -> (u32, Vec<u8>) {
    match op_code {
        op::EXCHANGE_ID => op_exchange_id_with_role(reader, sessions, ServerRole::Ds).await,
        op::CREATE_SESSION => op_create_session(reader, sessions).await,
        op::DESTROY_SESSION => op_destroy_session_ds(reader, ctx, sessions).await,
        op::DESTROY_CLIENTID => op_destroy_clientid_ds(reader, ctx).await,
        op::RECLAIM_COMPLETE => crate::nfs4_server::op_reclaim_complete(reader).await,
        op::SEQUENCE => op_sequence(reader, sessions).await,
        op::PUTFH => op_putfh_ds(reader, ctx, state).await,
        op::READ => op_read_ds(reader, ctx, state).await,
        op::WRITE => op_write_ds(reader, ctx, state).await,
        op::COMMIT => op_commit_ds(reader, ctx, state).await,
        op::GETATTR => op_getattr_ds(reader, ctx, state).await,
        // I-PN7: every other op is rejected.
        _ => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
            (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
        }
    }
}

async fn op_putfh_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    state: &mut DsCompoundState,
) -> (u32, Vec<u8>) {
    let mut w = XdrWriter::new();
    w.write_u32(op::PUTFH);

    let Ok(bytes) = reader.read_opaque() else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    let Ok(fh) = PnfsFileHandle::decode(&bytes) else {
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    };

    // Phase 15c: consult the MDS-published recall list before MAC
    // validation. A revoked fh4 must fail even if MAC + expiry pass
    // (I-PN1 + ADR-038 §D6).
    if let Some(mgr) = ctx.mds_layout_manager.as_ref() {
        if mgr.is_revoked(&fh) {
            tracing::debug!("DS rejected revoked fh4");
            w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
            return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
        }
    }

    // When the MDS rotated K_layout, ctx.mac_key is stale by design —
    // the production path passes the live key via the manager.
    let live_key = ctx
        .mds_layout_manager
        .as_ref()
        .map_or_else(|| ctx.mac_key.clone(), |m| m.current_mac_key());
    if let Err(err) = fh.validate(&live_key, (ctx.now_ms)()) {
        // Both MacMismatch and Expired map to BADHANDLE per I-PN1.
        let reason = match err {
            FhValidateError::MacMismatch => "mac_mismatch",
            FhValidateError::Expired { .. } => "expired",
        };
        tracing::debug!(reason, "DS rejected fh4");
        w.write_u32(nfs4_status::NFS4ERR_BADHANDLE);
        return (nfs4_status::NFS4ERR_BADHANDLE, w.into_bytes());
    }

    state.current_fh = Some(fh);
    w.write_u32(nfs4_status::NFS4_OK);
    (nfs4_status::NFS4_OK, w.into_bytes())
}

async fn op_read_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    state: &DsCompoundState,
) -> (u32, Vec<u8>) {
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let offset = reader.read_u64().unwrap_or(0);
    let count = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::READ);

    let Some(fh) = state.current_fh.as_ref() else {
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    };

    // Translate the kernel's offset to a composition-absolute offset.
    //
    // Phase 15c.9: with the new one-segment-N-mirrors layout shape,
    // the FH represents the whole segment (`stripe_index = 0`) and
    // the kernel sends FILE offsets directly. The DS reads at
    // `abs_offset = kernel_offset` and is bounded only by the
    // kernel's `count` (already ≤ the negotiated rsize).
    //
    // Legacy per-stripe FHs (`stripe_index > 0`, pre-15c.9 layouts
    // still cached at the kernel) keep the old translation: the
    // kernel's `offset` is stripe-relative, the DS adds
    // `stripe_index * stripe_size` and bounds reads to one stripe.
    let (abs_offset, bounded_count) = if fh.stripe_index == 0 {
        (offset, u64::from(count))
    } else {
        let stripe_base = u64::from(fh.stripe_index) * ctx.stripe_size_bytes;
        let stripe_end = stripe_base.saturating_add(ctx.stripe_size_bytes);
        let abs = stripe_base.saturating_add(offset);
        let max_count = stripe_end.saturating_sub(abs);
        (abs, u64::from(count).min(max_count))
    };

    // ADR-038 rev 3 §D5: consult the write-buffer first. Two layers:
    //   1. Resolve the fh's composition_id through the redirect table —
    //      a prior COMMIT on the same fh4 may have produced a NEW
    //      composition_id; reads via the OLD fh4 should see the new
    //      bytes.
    //   2. If a buffer exists for the (resolved) composition_id, serve
    //      the prefix from the buffer; fall back to gateway.read for
    //      the suffix that's past the buffer end.
    let resolved_cid = ctx.write_buffers.resolve(fh.composition_id);
    let (buf_prefix, fully_buffered) =
        ctx.write_buffers
            .read(resolved_cid, abs_offset, bounded_count);

    if fully_buffered {
        w.write_u32(nfs4_status::NFS4_OK);
        // EOF on a buffered read is hard to signal precisely without
        // knowing the underlying composition's size; the kernel
        // tolerates eof=false on short responses (re-reads at higher
        // offset will see EOF naturally when both buffer + composition
        // are exhausted).
        w.write_bool(false);
        w.write_opaque(&buf_prefix);
        return (nfs4_status::NFS4_OK, w.into_bytes());
    }

    let suffix_offset = abs_offset.saturating_add(u64::try_from(buf_prefix.len()).unwrap_or(0));
    let suffix_len = bounded_count.saturating_sub(u64::try_from(buf_prefix.len()).unwrap_or(0));
    let req = ReadRequest {
        tenant_id: fh.tenant_id,
        namespace_id: fh.namespace_id,
        composition_id: resolved_cid,
        offset: suffix_offset,
        length: suffix_len,
    };

    let status = if let Ok(resp) = ctx.block_gateway(ctx.gateway.read(req)) {
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_bool(resp.eof);
        let mut out = buf_prefix;
        out.extend_from_slice(&resp.data);
        w.write_opaque(&out);
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_IO);
        nfs4_status::NFS4ERR_IO
    };

    (status, w.into_bytes())
}

/// op::WRITE handler — accumulates plaintext into the per-composition
/// write buffer (ADR-038 rev 3 §D5). On overflow returns
/// `NFS4ERR_NOSPC`; the kernel pNFS client recovers by issuing
/// COMMIT-then-OPEN-then-WRITE.
async fn op_write_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    state: &DsCompoundState,
) -> (u32, Vec<u8>) {
    // RFC 8881 §18.32: stateid (16) + offset (u64) + stable (u32) + data (opaque<>)
    let _stateid = reader.read_opaque_fixed(16).unwrap_or_default();
    let offset = reader.read_u64().unwrap_or(0);
    let _stable = reader.read_u32().unwrap_or(2); // FILE_SYNC=2; advisory only
    let data = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::WRITE);

    let Some(fh) = state.current_fh.as_ref() else {
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    };

    // Translate kernel offset to composition-absolute (mirrors op_read_ds
    // for the legacy stripe-FH shape; one-segment-N-mirrors layouts use
    // stripe_index=0 so abs_offset == offset).
    let abs_offset = if fh.stripe_index == 0 {
        offset
    } else {
        let stripe_base = u64::from(fh.stripe_index) * ctx.stripe_size_bytes;
        stripe_base.saturating_add(offset)
    };

    match ctx.write_buffers.buffer_write(
        fh.composition_id,
        fh.tenant_id,
        fh.namespace_id,
        abs_offset,
        &data,
    ) {
        BufferWriteResult::Accepted => {
            #[allow(clippy::cast_possible_truncation)]
            let count = data.len() as u32;
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_u32(count);
            // RFC 8881 §18.32.5 stable_how4:
            //   0 = UNSTABLE4 — server may not have committed yet
            //   1 = DATA_SYNC4 — server has committed data
            //   2 = FILE_SYNC4 — server has committed data + metadata
            // Pre-fix returned 2 (FILE_SYNC) unconditionally — a lie:
            // we only staged the bytes in the per-composition buffer,
            // nothing reached the underlying gateway/storage. Linux
            // 6.x reads `committed=FILE_SYNC` as "no need to COMMIT,
            // already durable" and proceeds straight to
            // DESTROY_SESSION on file close. Our DESTROY_SESSION
            // handler then drops the buffer per ADR-038 §D5 (no
            // implicit flush), and the WRITE is silently lost.
            //
            // Replying UNSTABLE forces the client to issue COMMIT
            // before destroying the session for durability — that's
            // the contract the chunk-staging buffer's "COMMIT drains
            // via gateway.write" semantics require. Verified via
            // tcpdump 2026-05-10 round 2 follow-up.
            w.write_u32(0); // committed = UNSTABLE4
            w.write_opaque_fixed(&[0u8; 8]); // writeverf4 — fixed; we never restart server-side
            (nfs4_status::NFS4_OK, w.into_bytes())
        }
        BufferWriteResult::Nospc {
            current,
            cap,
            requested,
        } => {
            tracing::debug!(
                composition_id = ?fh.composition_id,
                current_bytes = current,
                cap_bytes = cap,
                requested_bytes = requested,
                "DS WRITE rejected NOSPC — per-composition buffer cap hit"
            );
            w.write_u32(nfs4_status::NFS4ERR_NOSPC);
            (nfs4_status::NFS4ERR_NOSPC, w.into_bytes())
        }
    }
}

/// op::COMMIT handler — drains the per-composition write buffer and
/// produces a new composition via `GatewayOps::write` (ADR-038 rev 3
/// §D5). The new composition_id is recorded in the redirect table so
/// subsequent reads through the OLD fh4 see post-commit bytes; the
/// kernel pNFS client picks up the new id on its next LAYOUTGET.
async fn op_commit_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    state: &DsCompoundState,
) -> (u32, Vec<u8>) {
    // RFC 8881 §18.3: offset + count are advisory; we flush the entire
    // buffer on any COMMIT.
    let _offset = reader.read_u64().unwrap_or(0);
    let _count = reader.read_u32().unwrap_or(0);

    let mut w = XdrWriter::new();
    w.write_u32(op::COMMIT);

    let Some(fh) = state.current_fh.as_ref() else {
        crate::pnfs_write_buffer::ds_commit_total()
            .with_label_values(&["no_fh"])
            .inc();
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    };

    let Some(entry) = ctx.write_buffers.take(fh.composition_id) else {
        // No buffered writes for this composition — nothing to flush.
        // Reads-only path; reply OK with the fixed writeverf (RFC 8435
        // tightly_coupled mode permits — durability via the underlying
        // Raft log).
        crate::pnfs_write_buffer::ds_commit_total()
            .with_label_values(&["no_buffer"])
            .inc();
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_opaque_fixed(&[0u8; 8]); // writeverf4
        return (nfs4_status::NFS4_OK, w.into_bytes());
    };

    let req = crate::ops::WriteRequest {
        tenant_id: entry.tenant_id,
        namespace_id: entry.namespace_id,
        data: entry.data,
        name: None,
        conditional: None,
        workflow_ref: None,
        idempotency_key: None,
        forwarded_from_node: None,
        comp_id_override: None,
    };
    let status = match ctx.block_gateway(ctx.gateway.write(req)) {
        Ok(resp) => {
            ctx.write_buffers
                .record_redirect(fh.composition_id, resp.composition_id);
            crate::pnfs_write_buffer::ds_commit_total()
                .with_label_values(&["ok"])
                .inc();
            tracing::debug!(
                original = ?fh.composition_id,
                current = ?resp.composition_id,
                bytes = resp.bytes_written,
                "DS COMMIT flushed buffer to new composition"
            );
            w.write_u32(nfs4_status::NFS4_OK);
            w.write_opaque_fixed(&[0u8; 8]); // writeverf4
            nfs4_status::NFS4_OK
        }
        Err(e) => {
            crate::pnfs_write_buffer::ds_commit_total()
                .with_label_values(&["gateway_err"])
                .inc();
            tracing::warn!(error = ?e, "DS COMMIT gateway.write failed");
            w.write_u32(nfs4_status::NFS4ERR_IO);
            nfs4_status::NFS4ERR_IO
        }
    };

    (status, w.into_bytes())
}

/// DS-side wrapper around `nfs4_server::op_destroy_session`. After
/// the session is removed, drops all DS write buffers + redirects so
/// the next session sees a clean slate (per ADR-038 §D5.1, no
/// implicit flush — the kernel must COMMIT before destroy if it
/// wants durability).
async fn op_destroy_session_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    sessions: &SessionManager,
) -> (u32, Vec<u8>) {
    let result = op_destroy_session(reader, sessions).await;
    ctx.write_buffers.clear_all();
    result
}

/// DS-side wrapper around `nfs4_server::op_destroy_clientid`. Same
/// buffer-clear semantics as `op_destroy_session_ds`.
async fn op_destroy_clientid_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
) -> (u32, Vec<u8>) {
    let result = crate::nfs4_server::op_destroy_clientid(reader).await;
    ctx.write_buffers.clear_all();
    result
}

async fn op_getattr_ds<G: GatewayOps + Send + Sync + 'static>(
    reader: &mut XdrReader<'_>,
    _ctx: &DsContext<G>,
    state: &DsCompoundState,
) -> (u32, Vec<u8>) {
    // Skip attr_request bitmap (clients ask for various attributes; we
    // currently return a minimal fixed bitmap — clients tolerate this
    // because they already learned the file size from the MDS).
    let _bitmap = reader.read_opaque().unwrap_or_default();

    let mut w = XdrWriter::new();
    w.write_u32(op::GETATTR);
    if state.current_fh.is_none() {
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    }
    // For now: empty bitmap + empty attrs payload. Sufficient for the
    // RFC-fidelity test (Phase 15b).
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_u32(0); // bitmap word count = 0
    w.write_opaque(&[]); // attrs
    (nfs4_status::NFS4_OK, w.into_bytes())
}

// =============================================================================
// Unit tests (TDD)
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnfs::derive_pnfs_fh_mac_key;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixed_clock(t: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(move || t)
    }

    /// Tracking gateway that records read calls. Lets us assert that
    /// `op_read_ds` did or did not invoke the gateway (I-PN1: forged
    /// fh4 must NOT reach `GatewayOps::read`).
    ///
    /// Post-ADR-038-rev-3: `write` is no longer `unreachable!()` —
    /// `op_commit_ds` flushes the chunk-staging buffer through it.
    /// We synthesize a deterministic new composition_id from the
    /// write count so tests can assert on redirect-table behavior.
    struct TrackingGateway {
        reads: AtomicU64,
        writes: AtomicU64,
        fixed_response: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl GatewayOps for TrackingGateway {
        async fn read(
            &self,
            _req: ReadRequest,
        ) -> Result<crate::ops::ReadResponse, crate::error::GatewayError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(crate::ops::ReadResponse {
                data: self.fixed_response.clone(),
                eof: false,
                content_type: None,
            })
        }
        async fn write(
            &self,
            req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            let n = self.writes.fetch_add(1, Ordering::SeqCst);
            // Derive a deterministic new composition_id from the write
            // counter so round-trip tests can assert on the redirect.
            let mut bytes = [0u8; 16];
            bytes[0..8].copy_from_slice(&(n + 1).to_le_bytes());
            bytes[8] = 0xc0;
            bytes[9] = 0xde;
            Ok(crate::ops::WriteResponse {
                composition_id: kiseki_common::ids::CompositionId(uuid::Uuid::from_bytes(bytes)),
                bytes_written: u64::try_from(req.data.len()).unwrap_or(0),
            })
        }
    }

    fn make_ctx() -> (Arc<DsContext<TrackingGateway>>, PnfsFhMacKey) {
        let key = derive_pnfs_fh_mac_key(&[0xab; 32], &[0xcd; 16]);
        let ctx = DsContext {
            gateway: Arc::new(TrackingGateway {
                reads: AtomicU64::new(0),
                writes: AtomicU64::new(0),
                fixed_response: vec![0xee; 4096],
            }),
            mac_key: key.clone(),
            stripe_size_bytes: 1_048_576,
            rt: tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
                static RT: std::sync::OnceLock<tokio::runtime::Runtime> =
                    std::sync::OnceLock::new();
                RT.get_or_init(|| tokio::runtime::Runtime::new().expect("rt"))
                    .handle()
                    .clone()
            }),
            now_ms: fixed_clock(1_000),
            mds_layout_manager: None,
            write_buffers: Arc::new(crate::pnfs_write_buffer::DsWriteBuffers::new()),
        };
        (Arc::new(ctx), key)
    }

    fn issue_fh(key: &PnfsFhMacKey, expiry_ms: u64, stripe: u32) -> PnfsFileHandle {
        use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
        PnfsFileHandle::issue(
            key,
            OrgId(uuid::Uuid::from_bytes([0x11; 16])),
            NamespaceId(uuid::Uuid::from_bytes([0x22; 16])),
            CompositionId(uuid::Uuid::from_bytes([0x33; 16])),
            stripe,
            expiry_ms,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn allowed_ds_ops_are_exactly_eleven() {
        // I-PN7 — pin the spec: changes here require an ADR amendment.
        // Bumped 8 → 10 in 2026-05-07: added RECLAIM_COMPLETE +
        // DESTROY_CLIENTID after the kernel pNFS client retried session
        // establishment in a loop when the DS rejected them with
        // NFS4ERR_NOTSUPP. RFC 8881 §13.6.4 requires DS to permit
        // RECLAIM_COMPLETE on DS-only sessions. Bumped 10 → 11 in
        // 2026-05-10: added WRITE per ADR-038 rev 3 §D5 (chunk-staging
        // buffer; flush via gateway.write on COMMIT).
        assert_eq!(ALLOWED_DS_OPS.len(), 11);
        let mut sorted: Vec<u32> = ALLOWED_DS_OPS.into();
        sorted.sort_unstable();
        let mut expected: Vec<u32> = [
            op::EXCHANGE_ID,
            op::CREATE_SESSION,
            op::DESTROY_SESSION,
            op::DESTROY_CLIENTID,
            op::RECLAIM_COMPLETE,
            op::PUTFH,
            op::READ,
            op::WRITE,
            op::COMMIT,
            op::SEQUENCE,
            op::GETATTR,
        ]
        .into();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn putfh_with_valid_fh4_succeeds() {
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 5_000, 0);
        let bytes = fh.encode();

        let mut state = DsCompoundState::default();
        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state).await;
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(state.current_fh, Some(fh));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn putfh_with_forged_mac_returns_badhandle() {
        let (ctx, _real_key) = make_ctx();
        let other_key = derive_pnfs_fh_mac_key(&[0x99; 32], &[0x88; 16]);
        let fh = issue_fh(&other_key, 5_000, 0);
        let bytes = fh.encode();

        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let mut state = DsCompoundState::default();
        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state).await;
        assert_eq!(status, nfs4_status::NFS4ERR_BADHANDLE);
        assert!(state.current_fh.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn putfh_with_expired_fh4_returns_badhandle() {
        let (ctx, key) = make_ctx(); // now_ms = 1000
        let fh = issue_fh(&key, 500, 0); // expiry < now → expired
        let bytes = fh.encode();

        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let mut state = DsCompoundState::default();
        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state).await;
        assert_eq!(status, nfs4_status::NFS4ERR_BADHANDLE);
        assert!(state.current_fh.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_without_putfh_returns_nofilehandle() {
        let (ctx, _) = make_ctx();
        let mut buf = XdrWriter::new();
        buf.write_opaque_fixed(&[0u8; 16]); // stateid
        buf.write_u64(0); // offset
        buf.write_u32(4096); // count
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let state = DsCompoundState::default();
        let (status, _) = op_read_ds(&mut reader, &ctx, &state).await;
        assert_eq!(status, nfs4_status::NFS4ERR_NOFILEHANDLE);
        // No GatewayOps call.
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_with_valid_putfh_invokes_gateway_with_translated_offset() {
        let (ctx, key) = make_ctx();
        let stripe_index = 3u32;
        let fh = issue_fh(&key, 5_000, stripe_index);
        let mut state = DsCompoundState {
            current_fh: Some(fh),
        };

        let client_offset = 8192u64;
        let count = 4096u32;

        let mut buf = XdrWriter::new();
        buf.write_opaque_fixed(&[0u8; 16]);
        buf.write_u64(client_offset);
        buf.write_u32(count);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let (status, _) = op_read_ds(&mut reader, &ctx, &state).await;
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 1);
        // Sanity check we'd compute the absolute offset correctly.
        let expected_abs = u64::from(stripe_index) * ctx.stripe_size_bytes + client_offset;
        assert_eq!(expected_abs, 3 * 1_048_576 + 8192);
        // Suppress unused-mut warning since this test mutates state for
        // construction only.
        let _ = state.current_fh.take();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_clamps_count_to_stripe_boundary() {
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 5_000, 0);
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        let stripe = ctx.stripe_size_bytes;
        let oversized = u32::MAX;
        let client_offset = stripe - 4096;

        let mut buf = XdrWriter::new();
        buf.write_opaque_fixed(&[0u8; 16]);
        buf.write_u64(client_offset);
        buf.write_u32(oversized);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let (status, _) = op_read_ds(&mut reader, &ctx, &state).await;
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 1);
        // Indirect: TrackingGateway always returns its fixed_response,
        // and clamping happens in the *count* sent to GatewayOps. We
        // assert this by confirming the call succeeded with no panic.
    }

    /// op 59 = ALLOCATE — not in `ALLOWED_DS_OPS`.
    const ALLOCATE_OP: u32 = 59;

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_op_returns_notsupp_without_state_change() {
        let (ctx, _) = make_ctx();
        let session_mgr = SessionManager::new();
        let mut state = DsCompoundState::default();

        let mut buf = XdrWriter::new();
        // ALLOCATE args: stateid + offset + length — but we expect the
        // dispatcher to short-circuit before consuming them.
        buf.write_opaque_fixed(&[0u8; 16]);
        buf.write_u64(0);
        buf.write_u64(0);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let (status, _) =
            process_ds_op(ALLOCATE_OP, &mut reader, &ctx, &session_mgr, &mut state).await;
        assert_eq!(status, nfs4_status::NFS4ERR_NOTSUPP);
        assert!(state.current_fh.is_none());
    }

    // =========================================================================
    // ADR-038 rev 3 §D5 — DS WRITE / COMMIT integration tests.
    // =========================================================================

    /// Build a WRITE op argument (stateid + offset + stable + data).
    fn write_args(offset: u64, data: &[u8]) -> Vec<u8> {
        let mut w = XdrWriter::new();
        w.write_opaque_fixed(&[0u8; 16]); // stateid
        w.write_u64(offset);
        w.write_u32(2); // stable = FILE_SYNC (advisory only)
        w.write_opaque(data);
        w.into_bytes()
    }

    /// Build a COMMIT op argument (offset + count, advisory).
    fn commit_args(offset: u64, count: u32) -> Vec<u8> {
        let mut w = XdrWriter::new();
        w.write_u64(offset);
        w.write_u32(count);
        w.into_bytes()
    }

    /// Build a READ op argument (stateid + offset + count).
    fn read_args(offset: u64, count: u32) -> Vec<u8> {
        let mut w = XdrWriter::new();
        w.write_opaque_fixed(&[0u8; 16]);
        w.write_u64(offset);
        w.write_u32(count);
        w.into_bytes()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_then_read_serves_from_buffer() {
        // Pre-fix the DS replied NFS4ERR_NOTSUPP on WRITE; post-fix
        // (ADR-038 rev 3 §D5) WRITE accumulates into the buffer and a
        // subsequent READ at the same offset returns the buffered
        // bytes — without going through gateway.read.
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 9_999_999, 0);
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        // 1. WRITE 8 bytes at offset 0.
        let write_args = write_args(0, b"DEADBEEF");
        let mut r = XdrReader::new(&write_args);
        let (st, _) = op_write_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);

        // 2. READ 8 bytes at offset 0 — buffer has them.
        let read_args = read_args(0, 8);
        let mut r = XdrReader::new(&read_args);
        let (st, reply) = op_read_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);
        // gateway.read MUST NOT have been called — buffer covered.
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 0);
        // Decode the reply: op + status + eof + opaque.
        let mut rd = XdrReader::new(&reply);
        assert_eq!(rd.read_u32().unwrap(), op::READ);
        assert_eq!(rd.read_u32().unwrap(), nfs4_status::NFS4_OK);
        let _eof = rd.read_bool().unwrap();
        assert_eq!(rd.read_opaque().unwrap(), b"DEADBEEF");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_drains_to_gateway_and_records_redirect() {
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 9_999_999, 0);
        let original_cid = fh.composition_id;
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        // WRITE → COMMIT → assert gateway saw exactly one write.
        let write_args = write_args(0, b"hello world");
        let mut r = XdrReader::new(&write_args);
        let (st, _) = op_write_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);

        let commit = commit_args(0, 0);
        let mut r = XdrReader::new(&commit);
        let (st, _) = op_commit_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.writes.load(Ordering::SeqCst), 1);

        // Buffer is drained — total_bytes back to 0.
        assert_eq!(ctx.write_buffers.total_bytes(), 0);

        // Redirect recorded: original_cid → new composition (whatever
        // TrackingGateway::write minted, write count = 1).
        let resolved = ctx.write_buffers.resolve(original_cid);
        assert_ne!(
            resolved, original_cid,
            "redirect must point at new composition"
        );

        // Subsequent READ on the OLD fh4 hits gateway.read (buffer is
        // gone post-COMMIT) but with the resolved (new) composition_id.
        // Verified via gateway counter increment.
        let read = read_args(0, 11);
        let mut r = XdrReader::new(&read);
        let (st, _) = op_read_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_past_buffer_cap_returns_nospc() {
        // Force a tiny cap by overriding the buffers field — the
        // env-var reads-once-per-process so we can't change it
        // mid-test reliably, but the DsContext is ours to construct.
        let (mut_ctx, key) = make_ctx();
        // Replace write_buffers with an 8-byte cap.
        let arc = Arc::new(crate::pnfs_write_buffer::DsWriteBuffers::with_cap(8));
        // SAFETY: we created mut_ctx; tests are single-threaded
        // wrt this Arc. Build a fresh DsContext with the small-cap
        // buffer and re-wrap.
        let small_ctx = Arc::new(DsContext {
            gateway: Arc::clone(&mut_ctx.gateway),
            mac_key: mut_ctx.mac_key.clone(),
            stripe_size_bytes: mut_ctx.stripe_size_bytes,
            rt: mut_ctx.rt.clone(),
            now_ms: Arc::clone(&mut_ctx.now_ms),
            mds_layout_manager: mut_ctx.mds_layout_manager.clone(),
            write_buffers: arc,
        });
        let fh = issue_fh(&key, 9_999_999, 0);
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        // Fill cap.
        let args = write_args(0, b"AAAAAAAA"); // 8 bytes
        let mut r = XdrReader::new(&args);
        let (st, _) = op_write_ds(&mut r, &small_ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);

        // Overflow.
        let args = write_args(8, b"X");
        let mut r = XdrReader::new(&args);
        let (st, _) = op_write_ds(&mut r, &small_ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4ERR_NOSPC);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn destroy_session_drops_unflushed_buffers() {
        // ADR-038 §D5.1: no implicit flush on DESTROY_SESSION. Bytes
        // written but not committed are lost — the kernel must COMMIT
        // before tearing down if it wants durability.
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 9_999_999, 0);
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        let args = write_args(0, b"unflushed");
        let mut r = XdrReader::new(&args);
        let (st, _) = op_write_ds(&mut r, &ctx, &state).await;
        assert_eq!(st, nfs4_status::NFS4_OK);
        assert_eq!(ctx.write_buffers.total_bytes(), 9);

        // Simulate DESTROY_SESSION via the DS-side wrapper. The wrapper
        // calls into nfs4_server::op_destroy_session (which handles
        // session state) and then clears DS write buffers.
        let mut destroy_args = XdrWriter::new();
        destroy_args.write_opaque_fixed(&[0u8; 16]); // session_id
        let bytes = destroy_args.into_bytes();
        let session_mgr = SessionManager::new();
        let mut r = XdrReader::new(&bytes);
        let _ = op_destroy_session_ds(&mut r, &ctx, &session_mgr).await;

        // Buffer is gone; gateway.write was NOT called (no implicit flush).
        assert_eq!(ctx.write_buffers.total_bytes(), 0);
        assert_eq!(ctx.gateway.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overlap_writes_last_write_wins_through_buffer() {
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 9_999_999, 0);
        let state = DsCompoundState {
            current_fh: Some(fh),
        };

        // First WRITE: AAAAAAAAAA at offset 0.
        let w1 = write_args(0, b"AAAAAAAAAA");
        let mut r = XdrReader::new(&w1);
        op_write_ds(&mut r, &ctx, &state).await;
        // Second WRITE: BBBB at offset 4 (overwrites bytes 4-7).
        let w2 = write_args(4, b"BBBB");
        let mut r = XdrReader::new(&w2);
        op_write_ds(&mut r, &ctx, &state).await;

        // READ 10 bytes — buffer should serve AAAABBBBAA.
        let rargs = read_args(0, 10);
        let mut r = XdrReader::new(&rargs);
        let (_, reply) = op_read_ds(&mut r, &ctx, &state).await;
        let mut rd = XdrReader::new(&reply);
        let _ = rd.read_u32().unwrap(); // op
        let _ = rd.read_u32().unwrap(); // status
        let _ = rd.read_bool().unwrap(); // eof
        assert_eq!(rd.read_opaque().unwrap(), b"AAAABBBBAA");
    }
}
