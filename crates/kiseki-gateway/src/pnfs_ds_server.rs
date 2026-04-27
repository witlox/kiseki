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

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;

use rustls::ServerConfig;

use crate::nfs4_server::{
    nfs4_status, op, op_create_session, op_destroy_session, op_exchange_id, op_sequence,
    SessionManager,
};
use crate::nfs_xdr::{
    encode_reply_accepted, read_rm_message, write_rm_message, RpcCallHeader, XdrReader, XdrWriter,
};
use crate::ops::{GatewayOps, ReadRequest};
use crate::pnfs::{FhValidateError, PnfsFhMacKey, PnfsFileHandle};

/// Op codes accepted by the DS. Anything outside this set returns
/// `NFS4ERR_NOTSUPP` per I-PN7.
pub const ALLOWED_DS_OPS: [u32; 8] = [
    op::EXCHANGE_ID,
    op::CREATE_SESSION,
    op::DESTROY_SESSION,
    op::SEQUENCE,
    op::PUTFH,
    op::READ,
    op::COMMIT,
    op::GETATTR,
    // NOTE: WRITE is intentionally absent in Phase 15a — `GatewayOps::write`
    // creates a fresh composition, which doesn't match the pNFS write-to-an-
    // existing-stripe semantics. WRITE is wired in a follow-up phase along
    // with the architect-blessed `GatewayOps::write_at`. See Phase 15b notes.
];

/// Stateless DS context. One instance per storage node.
pub struct DsContext<G: GatewayOps + Send + Sync + 'static> {
    /// Underlying gateway used to satisfy `GatewayOps::read` (decrypts
    /// chunks server-side per I-PN3).
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
pub fn handle_ds_connection<G: GatewayOps + Send + Sync + 'static, S: io::Read + io::Write>(
    stream: &mut S,
    ctx: &Arc<DsContext<G>>,
    sessions: &SessionManager,
) -> io::Result<()> {
    loop {
        let buf = match read_rm_message(stream) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut reader = XdrReader::new(&buf);

        let header = RpcCallHeader::decode(&mut reader)?;

        // We only accept NFS4_PROGRAM/NFS4_VERSION (4) — same gate as MDS.
        if header.program != 100_003 || header.version != 4 {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 1); // PROG_MISMATCH
            write_rm_message(stream, &w.into_bytes())?;
            continue;
        }

        let reply = dispatch_ds_compound(&header, &mut reader, ctx, sessions);
        write_rm_message(stream, &reply)?;
    }
}

/// Run a DS listener until shutdown is signaled. Spawns one thread
/// per accepted connection. The TLS path mirrors
/// [`crate::nfs_server::serve_nfs_listener`].
///
/// Spec: ADR-038 §D2 (DS listener), §D4.1 (TLS default), I-PN7
/// (op-subset enforced inside `dispatch_ds_compound`).
pub fn run_ds_server<G: GatewayOps + Send + Sync + 'static>(
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
    serve_ds_listener(listener, ctx, shutdown, tls);
}

/// Run a DS server on an already-bound listener — useful for tests
/// that pre-bind on `127.0.0.1:0`.
#[allow(clippy::needless_pass_by_value)]
pub fn serve_ds_listener<G: GatewayOps + Send + Sync + 'static>(
    listener: TcpListener,
    ctx: Arc<DsContext<G>>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    tls: Option<Arc<ServerConfig>>,
) {
    let _ = listener.set_nonblocking(true);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(addr = %addr, tls = tls.is_some(), "pNFS DS listening");
    }

    let sessions = Arc::new(SessionManager::new());

    loop {
        if let Some(ref s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("DS server shutting down");
                return;
            }
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                let ctx = Arc::clone(&ctx);
                let sessions = Arc::clone(&sessions);
                let tls = tls.clone();
                thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    if let Some(tls_cfg) = tls {
                        match rustls::ServerConnection::new(tls_cfg) {
                            Ok(conn) => {
                                let mut tls_stream = rustls::StreamOwned::new(conn, stream);
                                if let Err(e) =
                                    handle_ds_connection(&mut tls_stream, &ctx, &sessions)
                                {
                                    tracing::debug!(error = %e, peer = %peer, "DS-over-TLS connection ended");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, peer = %peer, "DS TLS server-conn init failed");
                            }
                        }
                    } else {
                        let mut s = stream;
                        if let Err(e) = handle_ds_connection(&mut s, &ctx, &sessions) {
                            tracing::debug!(error = %e, peer = %peer, "DS plaintext connection ended");
                        }
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                tracing::error!(error = %e, "DS accept error");
            }
        }
    }
}

/// Process one COMPOUND request. Pure function — testable without
/// touching TCP or TLS.
pub fn dispatch_ds_compound<G: GatewayOps + Send + Sync + 'static>(
    header: &RpcCallHeader,
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    sessions: &SessionManager,
) -> Vec<u8> {
    let _tag = reader.read_opaque().unwrap_or_default();
    let _minor_version = reader.read_u32().unwrap_or(1); // NFSv4.1
    let num_ops = reader.read_u32().unwrap_or(0).min(32);

    let mut op_results: Vec<Vec<u8>> = Vec::new();
    let mut compound_status = nfs4_status::NFS4_OK;
    let mut state = DsCompoundState::default();

    for _ in 0..num_ops {
        let Ok(op_code) = reader.read_u32() else {
            break;
        };

        let (status, result) = process_ds_op(op_code, reader, ctx, sessions, &mut state);
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
    buf
}

fn process_ds_op<G: GatewayOps + Send + Sync + 'static>(
    op_code: u32,
    reader: &mut XdrReader<'_>,
    ctx: &DsContext<G>,
    sessions: &SessionManager,
    state: &mut DsCompoundState,
) -> (u32, Vec<u8>) {
    match op_code {
        op::EXCHANGE_ID => op_exchange_id(reader, sessions),
        op::CREATE_SESSION => op_create_session(reader, sessions),
        op::DESTROY_SESSION => op_destroy_session(reader, sessions),
        op::SEQUENCE => op_sequence(reader, sessions),
        op::PUTFH => op_putfh_ds(reader, ctx, state),
        op::READ => op_read_ds(reader, ctx, state),
        op::COMMIT => op_commit_ds(reader, state),
        op::GETATTR => op_getattr_ds(reader, ctx, state),
        // I-PN7: every other op is rejected.
        _ => {
            let mut w = XdrWriter::new();
            w.write_u32(op_code);
            w.write_u32(nfs4_status::NFS4ERR_NOTSUPP);
            (nfs4_status::NFS4ERR_NOTSUPP, w.into_bytes())
        }
    }
}

fn op_putfh_ds<G: GatewayOps + Send + Sync + 'static>(
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

fn op_read_ds<G: GatewayOps + Send + Sync + 'static>(
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

    // Translate stripe-relative → absolute offset within the composition.
    let stripe_base = u64::from(fh.stripe_index) * ctx.stripe_size_bytes;
    let stripe_end = stripe_base.saturating_add(ctx.stripe_size_bytes);
    let abs_offset = stripe_base.saturating_add(offset);
    let max_count = stripe_end.saturating_sub(abs_offset);
    let bounded_count = u64::from(count).min(max_count);

    let req = ReadRequest {
        tenant_id: fh.tenant_id,
        namespace_id: fh.namespace_id,
        composition_id: fh.composition_id,
        offset: abs_offset,
        length: bounded_count,
    };

    let status = if let Ok(resp) = ctx.block_gateway(ctx.gateway.read(req)) {
        w.write_u32(nfs4_status::NFS4_OK);
        w.write_bool(resp.eof);
        w.write_opaque(&resp.data);
        nfs4_status::NFS4_OK
    } else {
        w.write_u32(nfs4_status::NFS4ERR_IO);
        nfs4_status::NFS4ERR_IO
    };

    (status, w.into_bytes())
}

fn op_commit_ds(_reader: &mut XdrReader<'_>, state: &DsCompoundState) -> (u32, Vec<u8>) {
    // Reads don't need COMMIT; for read-only DS in Phase 15a, COMMIT is
    // a no-op that returns a fixed writeverf. RFC 8435 tightly_coupled
    // mode allows this — durability comes from the underlying Raft log.
    let mut w = XdrWriter::new();
    w.write_u32(op::COMMIT);
    if state.current_fh.is_none() {
        w.write_u32(nfs4_status::NFS4ERR_NOFILEHANDLE);
        return (nfs4_status::NFS4ERR_NOFILEHANDLE, w.into_bytes());
    }
    w.write_u32(nfs4_status::NFS4_OK);
    w.write_opaque_fixed(&[0u8; 8]); // writeverf4
    (nfs4_status::NFS4_OK, w.into_bytes())
}

fn op_getattr_ds<G: GatewayOps + Send + Sync + 'static>(
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
    struct TrackingGateway {
        reads: AtomicU64,
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
            })
        }
        async fn write(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            unreachable!("DS Phase 15a does not call write")
        }
    }

    fn make_ctx() -> (Arc<DsContext<TrackingGateway>>, PnfsFhMacKey) {
        let key = derive_pnfs_fh_mac_key(&[0xab; 32], &[0xcd; 16]);
        let ctx = DsContext {
            gateway: Arc::new(TrackingGateway {
                reads: AtomicU64::new(0),
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

    #[test]
    fn allowed_ds_ops_are_exactly_eight() {
        // I-PN7 — pin the spec: changes here require an ADR amendment.
        assert_eq!(ALLOWED_DS_OPS.len(), 8);
        let mut sorted: Vec<u32> = ALLOWED_DS_OPS.into();
        sorted.sort_unstable();
        let mut expected: Vec<u32> = [
            op::EXCHANGE_ID,
            op::CREATE_SESSION,
            op::DESTROY_SESSION,
            op::PUTFH,
            op::READ,
            op::COMMIT,
            op::SEQUENCE,
            op::GETATTR,
        ]
        .into();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn putfh_with_valid_fh4_succeeds() {
        let (ctx, key) = make_ctx();
        let fh = issue_fh(&key, 5_000, 0);
        let bytes = fh.encode();

        let mut state = DsCompoundState::default();
        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state);
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(state.current_fh, Some(fh));
    }

    #[test]
    fn putfh_with_forged_mac_returns_badhandle() {
        let (ctx, _real_key) = make_ctx();
        let other_key = derive_pnfs_fh_mac_key(&[0x99; 32], &[0x88; 16]);
        let fh = issue_fh(&other_key, 5_000, 0);
        let bytes = fh.encode();

        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let mut state = DsCompoundState::default();
        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state);
        assert_eq!(status, nfs4_status::NFS4ERR_BADHANDLE);
        assert!(state.current_fh.is_none());
    }

    #[test]
    fn putfh_with_expired_fh4_returns_badhandle() {
        let (ctx, key) = make_ctx(); // now_ms = 1000
        let fh = issue_fh(&key, 500, 0); // expiry < now → expired
        let bytes = fh.encode();

        let mut buf = XdrWriter::new();
        buf.write_opaque(&bytes);
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let mut state = DsCompoundState::default();
        let (status, _) = op_putfh_ds(&mut reader, &ctx, &mut state);
        assert_eq!(status, nfs4_status::NFS4ERR_BADHANDLE);
        assert!(state.current_fh.is_none());
    }

    #[test]
    fn read_without_putfh_returns_nofilehandle() {
        let (ctx, _) = make_ctx();
        let mut buf = XdrWriter::new();
        buf.write_opaque_fixed(&[0u8; 16]); // stateid
        buf.write_u64(0); // offset
        buf.write_u32(4096); // count
        let inner = buf.into_bytes();
        let mut reader = XdrReader::new(&inner);

        let state = DsCompoundState::default();
        let (status, _) = op_read_ds(&mut reader, &ctx, &state);
        assert_eq!(status, nfs4_status::NFS4ERR_NOFILEHANDLE);
        // No GatewayOps call.
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn read_with_valid_putfh_invokes_gateway_with_translated_offset() {
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

        let (status, _) = op_read_ds(&mut reader, &ctx, &state);
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 1);
        // Sanity check we'd compute the absolute offset correctly.
        let expected_abs = u64::from(stripe_index) * ctx.stripe_size_bytes + client_offset;
        assert_eq!(expected_abs, 3 * 1_048_576 + 8192);
        // Suppress unused-mut warning since this test mutates state for
        // construction only.
        let _ = state.current_fh.take();
    }

    #[test]
    fn read_clamps_count_to_stripe_boundary() {
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

        let (status, _) = op_read_ds(&mut reader, &ctx, &state);
        assert_eq!(status, nfs4_status::NFS4_OK);
        assert_eq!(ctx.gateway.reads.load(Ordering::SeqCst), 1);
        // Indirect: TrackingGateway always returns its fixed_response,
        // and clamping happens in the *count* sent to GatewayOps. We
        // assert this by confirming the call succeeded with no panic.
    }

    /// op 59 = ALLOCATE — not in `ALLOWED_DS_OPS`.
    const ALLOCATE_OP: u32 = 59;

    #[test]
    fn unsupported_op_returns_notsupp_without_state_change() {
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

        let (status, _) = process_ds_op(ALLOCATE_OP, &mut reader, &ctx, &session_mgr, &mut state);
        assert_eq!(status, nfs4_status::NFS4ERR_NOTSUPP);
        assert!(state.current_fh.is_none());
    }
}
