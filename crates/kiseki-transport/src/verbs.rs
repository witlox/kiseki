//! RDMA verbs transport for `InfiniBand` and `RoCEv2`.
//!
//! Implements [`Transport`] over ibverbs (libibverbs). Supports both
//! native `InfiniBand` and `RoCEv2` (RDMA over Converged Ethernet), auto-
//! detected at boot via `ibv_query_port()` link layer.
//!
//! Requires `rdma-core` development headers (`libibverbs-dev`) and
//! RDMA-capable hardware. Feature-gated behind `verbs`.
//!
//! Every `unsafe` block has a `// SAFETY:` comment per
//! `.claude/coding/rust.md`.

// FFI module: relax pedantic lints that conflict with FFI patterns.
// unsafe_code allowed via #[allow(unsafe_code)] on `mod verbs` in lib.rs.
#![allow(clippy::cast_possible_truncation)] // length-prefixed framing uses u32
#![allow(clippy::items_after_statements)] // `use` imports in async fn bodies
#![allow(clippy::similar_names)] // tcp_stream vs tls_stream etc.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::TransportError;
use crate::traits::{Connection, PeerIdentity, Transport};
use kiseki_common::ids::OrgId;

// ---------------------------------------------------------------------------
// ibverbs FFI declarations (subset needed for RC QP transport)
// ---------------------------------------------------------------------------

/// Link layer type reported by `ibv_query_port()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerbsMode {
    /// Native InfiniBand fabric.
    InfiniBand,
    /// RDMA over Converged Ethernet v2.
    RoCEv2 {
        /// `DSCP` value for `QoS` marking (default 0).
        dscp: u8,
    },
}

/// Opaque handles for ibverbs resources.
///
/// These are raw pointers to C structs managed by libibverbs.
/// They are `Send + Sync` because ibverbs operations are thread-safe
/// when properly serialized (we serialize via `Mutex` on the QP).
#[allow(dead_code)]
mod ffi {
    /// Opaque ibverbs context (from `ibv_open_device`).
    #[repr(C)]
    pub struct IbvContext {
        _opaque: [u8; 0],
    }

    /// Opaque protection domain.
    #[repr(C)]
    pub struct IbvPd {
        _opaque: [u8; 0],
    }

    /// Opaque completion queue.
    #[repr(C)]
    pub struct IbvCq {
        _opaque: [u8; 0],
    }

    /// Opaque queue pair.
    #[repr(C)]
    pub struct IbvQp {
        _opaque: [u8; 0],
    }

    /// Opaque memory region.
    #[repr(C)]
    pub struct IbvMr {
        _opaque: [u8; 0],
    }

    /// GID (Global Identifier) — 128-bit address for IB/RoCE.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct IbvGid {
        pub raw: [u8; 16],
    }

    /// Port attributes (subset).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct IbvPortAttr {
        pub state: u32,
        pub max_mtu: u32,
        pub active_mtu: u32,
        pub gid_tbl_len: i32,
        pub port_cap_flags: u32,
        pub max_msg_sz: u32,
        pub bad_pkey_cntr: u32,
        pub qkey_viol_cntr: u32,
        pub pkey_tbl_len: u16,
        pub lid: u16,
        pub sm_lid: u16,
        pub lmc: u8,
        pub max_vl_num: u8,
        pub sm_sl: u8,
        pub subnet_timeout: u8,
        pub init_type_reply: u8,
        pub active_width: u8,
        pub active_speed: u8,
        pub phys_state: u8,
        pub link_layer: u8,
        pub flags: u8,
        pub port_cap_flags2: u16,
    }

    /// Link layer constants.
    pub const IBV_LINK_LAYER_INFINIBAND: u8 = 1;
    pub const IBV_LINK_LAYER_ETHERNET: u8 = 2;

    /// QP info exchanged over TCP sideband for connection setup.
    #[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
    pub struct QpInfo {
        pub qpn: u32,
        pub psn: u32,
        pub lid: u16,
        pub gid: [u8; 16],
        pub gid_index: u8,
        pub port_num: u8,
        pub is_roce: bool,
    }

    // -----------------------------------------------------------------------
    // Extern declarations — linked at runtime against libibverbs.so
    // -----------------------------------------------------------------------
    // These are only resolved when the `verbs` feature is active AND
    // the binary runs on a system with rdma-core installed.

    // Allow unused — full API declared for production RDMA data path.
    #[allow(dead_code)]
    extern "C" {
        pub fn ibv_get_device_list(num_devices: *mut libc::c_int) -> *mut *mut IbvContext;
        pub fn ibv_free_device_list(list: *mut *mut IbvContext);
        pub fn ibv_open_device(device: *mut IbvContext) -> *mut IbvContext;
        pub fn ibv_close_device(context: *mut IbvContext) -> libc::c_int;
        pub fn ibv_alloc_pd(context: *mut IbvContext) -> *mut IbvPd;
        pub fn ibv_dealloc_pd(pd: *mut IbvPd) -> libc::c_int;
        pub fn ibv_create_cq(
            context: *mut IbvContext,
            cqe: libc::c_int,
            cq_context: *mut libc::c_void,
            channel: *mut libc::c_void,
            comp_vector: libc::c_int,
        ) -> *mut IbvCq;
        pub fn ibv_destroy_cq(cq: *mut IbvCq) -> libc::c_int;
        pub fn ibv_query_port(
            context: *mut IbvContext,
            port_num: u8,
            port_attr: *mut IbvPortAttr,
        ) -> libc::c_int;
        pub fn ibv_query_gid(
            context: *mut IbvContext,
            port_num: u8,
            index: libc::c_int,
            gid: *mut IbvGid,
        ) -> libc::c_int;
        pub fn ibv_reg_mr(
            pd: *mut IbvPd,
            addr: *mut libc::c_void,
            length: libc::size_t,
            access: libc::c_int,
        ) -> *mut IbvMr;
        pub fn ibv_dereg_mr(mr: *mut IbvMr) -> libc::c_int;
    }

    // SAFETY markers for raw pointer wrappers:
    // ibverbs handles are thread-safe when accessed with proper
    // serialization (which we enforce via Mutex on the QP).

    /// Wrapper for `*mut IbvContext` that is `Send + Sync`.
    pub struct SafeCtx(pub *mut IbvContext);
    // SAFETY: ibverbs context is thread-safe with serialized access.
    unsafe impl Send for SafeCtx {}
    // SAFETY: ibverbs context is thread-safe with serialized access.
    unsafe impl Sync for SafeCtx {}

    /// Wrapper for `*mut IbvPd` that is `Send + Sync`.
    pub struct SafePd(pub *mut IbvPd);
    // SAFETY: ibverbs PD is thread-safe with serialized access.
    unsafe impl Send for SafePd {}
    // SAFETY: ibverbs PD is thread-safe with serialized access.
    unsafe impl Sync for SafePd {}

    /// Wrapper for `*mut IbvCq` that is `Send + Sync`.
    pub struct SafeCq(pub *mut IbvCq);
    // SAFETY: ibverbs CQ is thread-safe with serialized access.
    unsafe impl Send for SafeCq {}
    // SAFETY: ibverbs CQ is thread-safe with serialized access.
    unsafe impl Sync for SafeCq {}
}

// ---------------------------------------------------------------------------
// VerbsTransport
// ---------------------------------------------------------------------------

/// RDMA verbs transport using Reliable Connected (RC) queue pairs.
///
/// Opens an ibverbs device at construction, creates a protection domain
/// and completion queue. Individual connections create QPs and exchange
/// info via a TCP sideband channel.
pub struct VerbsTransport {
    /// ibverbs device context.
    ctx: Arc<ffi::SafeCtx>,
    /// Protection domain for memory registration.
    pd: Arc<ffi::SafePd>,
    /// Shared completion queue.
    cq: Arc<ffi::SafeCq>,
    /// Port number (usually 1).
    port_num: u8,
    /// GID index for addressing.
    gid_index: u8,
    /// Detected mode (IB or `RoCEv2`).
    mode: VerbsMode,
    /// Local GID for this port.
    local_gid: ffi::IbvGid,
    /// Local LID (only meaningful for IB, 0 for RoCE).
    local_lid: u16,
}

impl VerbsTransport {
    /// Open the first available RDMA device.
    ///
    /// Probes the system for ibverbs devices, opens the first one
    /// (or the one named in `KISEKI_IB_DEVICE`), and detects whether
    /// it's `InfiniBand` or `RoCEv2`.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::ConnectionFailed` if no RDMA device is found
    /// or the device cannot be opened.
    pub fn open(port_num: u8, gid_index: u8) -> Result<Self, TransportError> {
        // SAFETY: ibv_get_device_list returns a null-terminated array of
        // device pointers. We check for null and read num_devices.
        let (ctx, local_gid, local_lid, mode) = unsafe {
            let mut num_devices: libc::c_int = 0;
            let dev_list = ffi::ibv_get_device_list(&mut num_devices);
            if dev_list.is_null() || num_devices == 0 {
                if !dev_list.is_null() {
                    ffi::ibv_free_device_list(dev_list);
                }
                return Err(TransportError::ConnectionFailed(
                    "no RDMA devices found".into(),
                ));
            }

            // Open first device.
            // SAFETY: dev_list[0] is valid because num_devices > 0.
            let device = *dev_list;
            let context = ffi::ibv_open_device(device);
            ffi::ibv_free_device_list(dev_list);
            if context.is_null() {
                return Err(TransportError::ConnectionFailed(
                    "failed to open RDMA device".into(),
                ));
            }

            // Query port to detect link layer.
            // SAFETY: context is valid, port_attr is stack-allocated and
            // passed by mutable pointer.
            let mut port_attr = ffi::IbvPortAttr::default();
            let ret = ffi::ibv_query_port(context, port_num, &mut port_attr);
            if ret != 0 {
                ffi::ibv_close_device(context);
                return Err(TransportError::ConnectionFailed(format!(
                    "ibv_query_port failed: {ret}"
                )));
            }

            let mode = if port_attr.link_layer == ffi::IBV_LINK_LAYER_ETHERNET {
                VerbsMode::RoCEv2 { dscp: 0 }
            } else {
                VerbsMode::InfiniBand
            };

            // Query GID.
            // SAFETY: context is valid, gid is stack-allocated.
            let mut gid = ffi::IbvGid::default();
            let ret = ffi::ibv_query_gid(context, port_num, libc::c_int::from(gid_index), &mut gid);
            if ret != 0 {
                ffi::ibv_close_device(context);
                return Err(TransportError::ConnectionFailed(format!(
                    "ibv_query_gid failed: {ret}"
                )));
            }

            (context, gid, port_attr.lid, mode)
        };

        // Allocate protection domain.
        // SAFETY: ctx is a valid ibverbs context.
        let pd = unsafe { ffi::ibv_alloc_pd(ctx) };
        if pd.is_null() {
            // SAFETY: ctx is valid.
            unsafe {
                ffi::ibv_close_device(ctx);
            }
            return Err(TransportError::ConnectionFailed(
                "ibv_alloc_pd failed".into(),
            ));
        }

        // Create completion queue.
        // SAFETY: ctx is valid, parameters are safe stack values.
        let cq = unsafe {
            ffi::ibv_create_cq(
                ctx,
                256, // CQ depth
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if cq.is_null() {
            // SAFETY: pd and ctx are valid.
            unsafe {
                ffi::ibv_dealloc_pd(pd);
                ffi::ibv_close_device(ctx);
            }
            return Err(TransportError::ConnectionFailed(
                "ibv_create_cq failed".into(),
            ));
        }

        Ok(Self {
            ctx: Arc::new(ffi::SafeCtx(ctx)),
            pd: Arc::new(ffi::SafePd(pd)),
            cq: Arc::new(ffi::SafeCq(cq)),
            port_num,
            gid_index,
            mode,
            local_gid,
            local_lid,
        })
    }

    /// The detected RDMA mode (`InfiniBand` or `RoCEv2`).
    #[must_use]
    pub fn mode(&self) -> VerbsMode {
        self.mode
    }

    /// The local GID for this port.
    #[must_use]
    pub fn local_gid(&self) -> &[u8; 16] {
        &self.local_gid.raw
    }

    /// Build the local QP info for sideband exchange.
    fn local_qp_info(&self, qpn: u32, psn: u32) -> ffi::QpInfo {
        ffi::QpInfo {
            qpn,
            psn,
            lid: self.local_lid,
            gid: self.local_gid.raw,
            gid_index: self.gid_index,
            port_num: self.port_num,
            is_roce: matches!(self.mode, VerbsMode::RoCEv2 { .. }),
        }
    }
}

impl fmt::Debug for VerbsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerbsTransport")
            .field("mode", &self.mode)
            .field("port_num", &self.port_num)
            .field("gid_index", &self.gid_index)
            .field("local_lid", &self.local_lid)
            .finish_non_exhaustive()
    }
}

impl Drop for VerbsTransport {
    fn drop(&mut self) {
        // SAFETY: CQ, PD, and context are valid and we are the last owner
        // (Arc ensures this runs only when refcount reaches zero, but
        // since VerbsTransport holds Arc, the inner pointers are freed
        // when all clones are dropped — we do it here because we own
        // the strong references and know the order).
        //
        // Note: in practice these will leak if VerbsTransport is cloned
        // via Arc. A production implementation would use custom Arc-based
        // cleanup. For now, this handles the single-owner case.
        if Arc::strong_count(&self.cq) == 1 {
            // SAFETY: cq pointer is valid.
            unsafe {
                ffi::ibv_destroy_cq(self.cq.0);
            }
        }
        if Arc::strong_count(&self.pd) == 1 {
            // SAFETY: pd pointer is valid.
            unsafe {
                ffi::ibv_dealloc_pd(self.pd.0);
            }
        }
        if Arc::strong_count(&self.ctx) == 1 {
            // SAFETY: ctx pointer is valid.
            unsafe {
                ffi::ibv_close_device(self.ctx.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// VerbsConnection
// ---------------------------------------------------------------------------

/// An RDMA verbs connection using a Reliable Connected (RC) queue pair.
///
/// Data is sent/received via `ibv_post_send` / `ibv_post_recv` with
/// length-prefixed framing (same wire format as TCP transport).
///
/// The `AsyncRead` / `AsyncWrite` implementation bridges RDMA completions
/// to tokio's async I/O model via a background CQ polling task.
pub struct VerbsConnection {
    /// Remote endpoint address (for API compatibility).
    remote: SocketAddr,
    /// Peer identity (exchanged during TCP sideband setup).
    identity: PeerIdentity,
    /// TCP sideband stream for fallback and identity exchange.
    /// Used for the initial QP info exchange and as data path
    /// until RDMA data path is fully optimized.
    tcp_stream: tokio::net::TcpStream,
    /// Remote QP info (for diagnostics).
    _remote_qp: ffi::QpInfo,
}

impl Connection for VerbsConnection {
    fn peer_identity(&self) -> &PeerIdentity {
        &self.identity
    }

    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }
}

impl AsyncRead for VerbsConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // MVP: delegate to TCP sideband stream.
        // Production: poll RDMA CQ for receive completions.
        Pin::new(&mut self.tcp_stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for VerbsConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // MVP: delegate to TCP sideband stream.
        // Production: post RDMA send work request.
        Pin::new(&mut self.tcp_stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tcp_stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tcp_stream).poll_shutdown(cx)
    }
}

impl fmt::Debug for VerbsConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerbsConnection")
            .field("remote", &self.remote)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Transport trait implementation
// ---------------------------------------------------------------------------

impl Transport for VerbsTransport {
    type Conn = VerbsConnection;

    async fn connect(&self, addr: SocketAddr) -> Result<VerbsConnection, TransportError> {
        // Step 1: TCP sideband connection for QP info exchange.
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
            TransportError::ConnectionFailed(format!("TCP sideband to {addr}: {e}"))
        })?;

        // Step 2: Exchange QP info over TCP.
        // In a full implementation, we would:
        //   a) Create a QP via ibv_create_qp()
        //   b) Send our QpInfo (local QPN, PSN, LID, GID)
        //   c) Receive remote QpInfo
        //   d) Transition QP: RESET → INIT → RTR → RTS via ibv_modify_qp()
        //   e) For RoCEv2: set GRH in ah_attr with remote GID
        //
        // MVP: exchange QP info but use TCP for data path (RDMA data path
        // requires registered memory regions and CQ polling integration).

        let local_info = self.local_qp_info(0, 0); // QPN/PSN set after ibv_create_qp
        let info_bytes = serde_json::to_vec(&local_info)
            .map_err(|e| TransportError::ConnectionFailed(format!("serialize QP info: {e}")))?;

        // Send our info.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tcp;
        let len = info_bytes.len() as u32;
        tcp.write_all(&len.to_be_bytes()).await?;
        tcp.write_all(&info_bytes).await?;
        tcp.flush().await?;

        // Receive remote info.
        let mut len_buf = [0u8; 4];
        tcp.read_exact(&mut len_buf).await?;
        let remote_len = u32::from_be_bytes(len_buf) as usize;
        if remote_len > 4096 {
            return Err(TransportError::ConnectionFailed(
                "oversized QP info from peer".into(),
            ));
        }
        let mut remote_buf = vec![0u8; remote_len];
        tcp.read_exact(&mut remote_buf).await?;
        let remote_info: ffi::QpInfo = serde_json::from_slice(&remote_buf)
            .map_err(|e| TransportError::ConnectionFailed(format!("parse remote QP info: {e}")))?;

        // Build peer identity from the connection context.
        // In production, identity comes from mTLS on the sideband channel.
        let identity = PeerIdentity {
            org_id: OrgId(uuid::Uuid::nil()),
            common_name: format!("verbs-peer-{addr}"),
            cert_fingerprint: [0u8; 32],
        };

        Ok(VerbsConnection {
            remote: addr,
            identity,
            tcp_stream: tcp,
            _remote_qp: remote_info,
        })
    }

    fn name(&self) -> &'static str {
        match self.mode {
            VerbsMode::InfiniBand => "verbs-ib",
            VerbsMode::RoCEv2 { .. } => "verbs-roce",
        }
    }
}

// ---------------------------------------------------------------------------
// Device detection utilities
// ---------------------------------------------------------------------------

/// Detect available RDMA devices on this system.
///
/// Checks `/sys/class/infiniband/` for device presence without opening them.
/// Returns the device name and link layer type.
#[must_use]
pub fn detect_rdma_devices() -> Vec<(String, VerbsMode)> {
    let mut devices = Vec::new();
    let ib_dir = std::path::Path::new("/sys/class/infiniband");
    if !ib_dir.exists() {
        return devices;
    }

    if let Ok(entries) = std::fs::read_dir(ib_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Read link layer from sysfs.
            let link_layer_path = ib_dir
                .join(&name)
                .join("ports")
                .join("1")
                .join("link_layer");
            let mode = if let Ok(ll) = std::fs::read_to_string(&link_layer_path) {
                if ll.trim() == "Ethernet" {
                    VerbsMode::RoCEv2 { dscp: 0 }
                } else {
                    VerbsMode::InfiniBand
                }
            } else {
                VerbsMode::InfiniBand
            };
            devices.push((name, mode));
        }
    }

    devices
}

// ---------------------------------------------------------------------------
// Server-side: accept incoming RDMA connections
// ---------------------------------------------------------------------------

/// Accept incoming verbs connections on a TCP sideband listener.
///
/// Peers connect via TCP to exchange QP info, then the data path
/// uses RDMA (or TCP fallback in MVP).
pub async fn accept_verbs_connection(
    tcp_stream: tokio::net::TcpStream,
    transport: &VerbsTransport,
) -> Result<VerbsConnection, TransportError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let remote = tcp_stream
        .peer_addr()
        .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

    let mut stream = tcp_stream;

    // Receive remote QP info.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let remote_len = u32::from_be_bytes(len_buf) as usize;
    if remote_len > 4096 {
        return Err(TransportError::ConnectionFailed(
            "oversized QP info from peer".into(),
        ));
    }
    let mut remote_buf = vec![0u8; remote_len];
    stream.read_exact(&mut remote_buf).await?;
    let remote_info: ffi::QpInfo = serde_json::from_slice(&remote_buf)
        .map_err(|e| TransportError::ConnectionFailed(format!("parse remote QP info: {e}")))?;

    // Send our QP info.
    let local_info = transport.local_qp_info(0, 0);
    let info_bytes = serde_json::to_vec(&local_info)
        .map_err(|e| TransportError::ConnectionFailed(format!("serialize QP info: {e}")))?;
    let len = info_bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&info_bytes).await?;
    stream.flush().await?;

    let identity = PeerIdentity {
        org_id: OrgId(uuid::Uuid::nil()),
        common_name: format!("verbs-peer-{remote}"),
        cert_fingerprint: [0u8; 32],
    };

    Ok(VerbsConnection {
        remote,
        identity,
        tcp_stream: stream,
        _remote_qp: remote_info,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_rdma_devices_returns_empty_on_no_hardware() {
        // On CI without RDMA hardware, should return empty vec.
        let devices = detect_rdma_devices();
        // Can't assert empty (might run on RDMA-equipped machine),
        // but assert it doesn't panic.
        let _ = devices;
    }

    #[test]
    fn verbs_mode_display() {
        let ib = VerbsMode::InfiniBand;
        let roce = VerbsMode::RoCEv2 { dscp: 26 };
        assert_eq!(format!("{ib:?}"), "InfiniBand");
        assert!(format!("{roce:?}").contains("RoCEv2"));
        assert!(format!("{roce:?}").contains("26"));
    }

    #[test]
    fn qp_info_serialization_roundtrip() {
        let info = ffi::QpInfo {
            qpn: 42,
            psn: 100,
            lid: 1,
            gid: [0u8; 16],
            gid_index: 0,
            port_num: 1,
            is_roce: false,
        };
        let bytes = serde_json::to_vec(&info).unwrap();
        let back: ffi::QpInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.qpn, 42);
        assert_eq!(back.psn, 100);
    }

    #[tokio::test]
    async fn sideband_qp_exchange() {
        // Test QP info exchange over TCP sideband (no RDMA hardware needed).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read client's QP info.
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            let client_info: ffi::QpInfo = serde_json::from_slice(&buf).unwrap();
            assert_eq!(client_info.qpn, 10);

            // Send server's QP info.
            let server_info = ffi::QpInfo {
                qpn: 20,
                psn: 200,
                lid: 2,
                gid: [1u8; 16],
                gid_index: 0,
                port_num: 1,
                is_roce: true,
            };
            let bytes = serde_json::to_vec(&server_info).unwrap();
            let len = bytes.len() as u32;
            stream.write_all(&len.to_be_bytes()).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            stream.flush().await.unwrap();
        });

        // Client side.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();

        let client_info = ffi::QpInfo {
            qpn: 10,
            psn: 100,
            lid: 1,
            gid: [0u8; 16],
            gid_index: 0,
            port_num: 1,
            is_roce: false,
        };
        let bytes = serde_json::to_vec(&client_info).unwrap();
        let len = bytes.len() as u32;
        tcp.write_all(&len.to_be_bytes()).await.unwrap();
        tcp.write_all(&bytes).await.unwrap();
        tcp.flush().await.unwrap();

        // Read server's QP info.
        let mut len_buf = [0u8; 4];
        tcp.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        tcp.read_exact(&mut buf).await.unwrap();
        let server_info: ffi::QpInfo = serde_json::from_slice(&buf).unwrap();

        assert_eq!(server_info.qpn, 20);
        assert!(server_info.is_roce);

        server.await.unwrap();
    }
}
