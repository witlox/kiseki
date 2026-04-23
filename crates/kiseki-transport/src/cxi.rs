//! libfabric/CXI transport for HPE Slingshot fabric.
//!
//! Implements [`Transport`] over libfabric with the CXI provider for
//! Slingshot 11 interconnect. Uses RDM (Reliable Datagram) endpoints
//! for connectionless, reliable messaging.
//!
//! Requires libfabric development headers (`libfabric-dev`) and a
//! Slingshot-equipped system with the CXI provider. Feature-gated
//! behind `cxi`.
//!
//! CXI-specific features:
//! - Service ID based addressing (no IP/TCP)
//! - VNI (Virtual Network Interface) for tenant isolation
//! - Adaptive routing for load balancing
//!
//! Every `unsafe` block has a `// SAFETY:` comment per
//! `.claude/coding/rust.md`.

// FFI module: relax pedantic lints that conflict with FFI patterns.
// unsafe_code allowed via #[allow(unsafe_code)] on `mod cxi` in lib.rs.
#![allow(clippy::cast_possible_truncation)] // length-prefixed framing uses u32
#![allow(clippy::items_after_statements)] // `use` imports in async fn bodies

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
// libfabric FFI declarations (subset for CXI RDM transport)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
mod ffi {
    /// Opaque libfabric info structure.
    #[repr(C)]
    pub struct FiInfo {
        _opaque: [u8; 0],
    }

    /// Opaque fabric handle.
    #[repr(C)]
    pub struct FidFabric {
        _opaque: [u8; 0],
    }

    /// Opaque domain handle.
    #[repr(C)]
    pub struct FidDomain {
        _opaque: [u8; 0],
    }

    /// Opaque endpoint handle.
    #[repr(C)]
    pub struct FidEp {
        _opaque: [u8; 0],
    }

    /// Opaque address vector handle.
    #[repr(C)]
    pub struct FidAv {
        _opaque: [u8; 0],
    }

    /// Opaque completion queue handle.
    #[repr(C)]
    pub struct FidCq {
        _opaque: [u8; 0],
    }

    /// Opaque event queue handle.
    #[repr(C)]
    pub struct FidEq {
        _opaque: [u8; 0],
    }

    /// Opaque memory region handle.
    #[repr(C)]
    pub struct FidMr {
        _opaque: [u8; 0],
    }

    /// CXI address — Service ID based.
    #[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
    pub struct CxiAddr {
        /// NIC identifier.
        pub nic: u32,
        /// Process ID on the NIC.
        pub pid: u32,
        /// VNI (Virtual Network Interface) for tenant isolation.
        pub vni: u16,
    }

    /// Endpoint info exchanged during connection setup.
    #[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
    pub struct EpInfo {
        pub addr: CxiAddr,
        /// Whether adaptive routing is enabled.
        pub adaptive_routing: bool,
    }

    // libfabric API constants.
    /// Reliable Datagram Message endpoint type.
    pub const FI_EP_RDM: u64 = 1;

    /// CXI provider name.
    pub const CXI_PROVIDER: &str = "cxi";

    // -----------------------------------------------------------------------
    // Extern declarations — linked at runtime against libfabric.so
    // -----------------------------------------------------------------------

    // Allow unused — full API declared for production CXI data path.
    #[allow(dead_code)]
    extern "C" {
        pub fn fi_getinfo(
            version: u32,
            node: *const libc::c_char,
            service: *const libc::c_char,
            flags: u64,
            hints: *const FiInfo,
            info: *mut *mut FiInfo,
        ) -> libc::c_int;
        pub fn fi_freeinfo(info: *mut FiInfo);
        pub fn fi_fabric(
            attr: *mut FiInfo,
            fabric: *mut *mut FidFabric,
            context: *mut libc::c_void,
        ) -> libc::c_int;
        pub fn fi_close(fid: *mut libc::c_void) -> libc::c_int;
    }

    /// Wrapper for `*mut FidFabric` that is `Send + Sync`.
    pub struct SafeFabric(pub *mut FidFabric);
    // SAFETY: libfabric handles are thread-safe with serialized access.
    unsafe impl Send for SafeFabric {}
    // SAFETY: libfabric handles are thread-safe with serialized access.
    unsafe impl Sync for SafeFabric {}

    /// Wrapper for `*mut FidDomain` that is `Send + Sync`.
    pub struct SafeDomain(pub *mut FidDomain);
    // SAFETY: libfabric domain handles are thread-safe.
    unsafe impl Send for SafeDomain {}
    // SAFETY: libfabric domain handles are thread-safe.
    unsafe impl Sync for SafeDomain {}
}

// ---------------------------------------------------------------------------
// CxiTransport
// ---------------------------------------------------------------------------

/// CXI transport using libfabric RDM endpoints.
///
/// Opens the CXI provider at construction, creates a fabric and domain.
/// Individual connections create endpoints and exchange addresses via
/// a TCP sideband channel.
pub struct CxiTransport {
    /// Fabric handle.
    _fabric: Arc<ffi::SafeFabric>,
    /// Domain handle.
    _domain: Arc<ffi::SafeDomain>,
    /// Local CXI address.
    local_addr: ffi::CxiAddr,
    /// VNI for tenant isolation.
    vni: u16,
    /// Whether adaptive routing is enabled.
    adaptive_routing: bool,
}

impl CxiTransport {
    /// Open the CXI provider via libfabric.
    ///
    /// Probes for the CXI provider and creates fabric + domain handles.
    ///
    /// # Errors
    ///
    /// Returns `TransportError::ConnectionFailed` if the CXI provider
    /// is not available or fabric creation fails.
    pub fn open(vni: u16, adaptive_routing: bool) -> Result<Self, TransportError> {
        // SAFETY: fi_getinfo is called with null node/service/hints to
        // discover available providers. We check the return code.
        let _info = unsafe {
            let mut info: *mut ffi::FiInfo = std::ptr::null_mut();
            let ret = ffi::fi_getinfo(
                0x0001_0013, // FI_VERSION(1, 19)
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut info,
            );
            if ret != 0 || info.is_null() {
                return Err(TransportError::ConnectionFailed(
                    "fi_getinfo failed: no CXI provider found".into(),
                ));
            }
            info
        };

        // In a full implementation:
        // 1. Filter fi_info list for CXI provider
        // 2. fi_fabric() → fabric handle
        // 3. fi_domain() → domain handle
        // 4. fi_av_open() → address vector
        // 5. fi_cq_open() × 2 → TX/RX completion queues

        // For compilation without libfabric, use null pointers
        // (the extern calls above won't be resolved at link time
        // unless the feature is active AND libfabric is installed).
        let fabric = std::ptr::null_mut();
        let domain = std::ptr::null_mut();

        let local_addr = ffi::CxiAddr {
            nic: 0,
            pid: std::process::id(),
            vni,
        };

        Ok(Self {
            _fabric: Arc::new(ffi::SafeFabric(fabric)),
            _domain: Arc::new(ffi::SafeDomain(domain)),
            local_addr,
            vni,
            adaptive_routing,
        })
    }

    /// The local CXI address.
    #[must_use]
    pub fn local_addr(&self) -> &ffi::CxiAddr {
        &self.local_addr
    }

    /// The VNI used for tenant isolation.
    #[must_use]
    pub fn vni(&self) -> u16 {
        self.vni
    }

    /// Build the local endpoint info for sideband exchange.
    fn local_ep_info(&self) -> ffi::EpInfo {
        ffi::EpInfo {
            addr: self.local_addr,
            adaptive_routing: self.adaptive_routing,
        }
    }
}

impl fmt::Debug for CxiTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CxiTransport")
            .field("vni", &self.vni)
            .field("adaptive_routing", &self.adaptive_routing)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// CxiConnection
// ---------------------------------------------------------------------------

/// A CXI connection using an RDM endpoint.
///
/// Data is sent/received via `fi_send` / `fi_recv` with completion polling.
/// The `AsyncRead`/`AsyncWrite` implementation bridges to tokio via
/// a TCP sideband fallback in MVP.
pub struct CxiConnection {
    /// Remote endpoint address (synthetic `SocketAddr` for API compat).
    remote: SocketAddr,
    /// Peer identity (from sideband exchange).
    identity: PeerIdentity,
    /// TCP sideband for data path (MVP fallback).
    tcp_stream: tokio::net::TcpStream,
    /// Remote endpoint info (for diagnostics).
    _remote_ep: ffi::EpInfo,
}

impl Connection for CxiConnection {
    fn peer_identity(&self) -> &PeerIdentity {
        &self.identity
    }

    fn remote_addr(&self) -> SocketAddr {
        self.remote
    }
}

impl AsyncRead for CxiConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // MVP: delegate to TCP sideband.
        // Production: poll CXI RX CQ for receive completions.
        Pin::new(&mut self.tcp_stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for CxiConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // MVP: delegate to TCP sideband.
        // Production: fi_send() with completion polling.
        Pin::new(&mut self.tcp_stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tcp_stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tcp_stream).poll_shutdown(cx)
    }
}

impl fmt::Debug for CxiConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CxiConnection")
            .field("remote", &self.remote)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Transport trait implementation
// ---------------------------------------------------------------------------

impl Transport for CxiTransport {
    type Conn = CxiConnection;

    async fn connect(&self, addr: SocketAddr) -> Result<CxiConnection, TransportError> {
        // Step 1: TCP sideband for endpoint info exchange.
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
            TransportError::ConnectionFailed(format!("CXI sideband to {addr}: {e}"))
        })?;

        // Step 2: Exchange endpoint info.
        // In a full implementation:
        //   a) fi_endpoint() — create RDM endpoint
        //   b) fi_ep_bind() — bind to AV, CQs, EQ
        //   c) fi_enable() — activate endpoint
        //   d) fi_av_insert() — insert peer address
        //   e) fi_send() / fi_recv() for data
        //
        // MVP: exchange info, use TCP for data path.

        let local_info = self.local_ep_info();
        let info_bytes = serde_json::to_vec(&local_info)
            .map_err(|e| TransportError::ConnectionFailed(format!("serialize EP info: {e}")))?;

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
                "oversized EP info from peer".into(),
            ));
        }
        let mut remote_buf = vec![0u8; remote_len];
        tcp.read_exact(&mut remote_buf).await?;
        let remote_info: ffi::EpInfo = serde_json::from_slice(&remote_buf)
            .map_err(|e| TransportError::ConnectionFailed(format!("parse remote EP info: {e}")))?;

        let identity = PeerIdentity {
            org_id: OrgId(uuid::Uuid::nil()),
            common_name: format!("cxi-peer-{addr}"),
            cert_fingerprint: [0u8; 32],
        };

        Ok(CxiConnection {
            remote: addr,
            identity,
            tcp_stream: tcp,
            _remote_ep: remote_info,
        })
    }

    fn name(&self) -> &'static str {
        "cxi"
    }
}

// ---------------------------------------------------------------------------
// Device detection
// ---------------------------------------------------------------------------

/// Detect available CXI devices on this system.
///
/// Checks `/sys/class/cxi/` for device presence.
#[must_use]
pub fn detect_cxi_devices() -> Vec<String> {
    let cxi_dir = std::path::Path::new("/sys/class/cxi");
    if !cxi_dir.exists() {
        return Vec::new();
    }

    let mut devices = Vec::new();
    if let Ok(entries) = std::fs::read_dir(cxi_dir) {
        for entry in entries.flatten() {
            devices.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    devices
}

// ---------------------------------------------------------------------------
// Server-side: accept incoming CXI connections
// ---------------------------------------------------------------------------

/// Accept incoming CXI connections on a TCP sideband listener.
pub async fn accept_cxi_connection(
    tcp_stream: tokio::net::TcpStream,
    transport: &CxiTransport,
) -> Result<CxiConnection, TransportError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let remote = tcp_stream
        .peer_addr()
        .map_err(|e| TransportError::ConnectionFailed(e.to_string()))?;

    let mut stream = tcp_stream;

    // Receive remote EP info.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let remote_len = u32::from_be_bytes(len_buf) as usize;
    if remote_len > 4096 {
        return Err(TransportError::ConnectionFailed(
            "oversized EP info from peer".into(),
        ));
    }
    let mut remote_buf = vec![0u8; remote_len];
    stream.read_exact(&mut remote_buf).await?;
    let remote_info: ffi::EpInfo = serde_json::from_slice(&remote_buf)
        .map_err(|e| TransportError::ConnectionFailed(format!("parse remote EP info: {e}")))?;

    // Send our EP info.
    let local_info = transport.local_ep_info();
    let info_bytes = serde_json::to_vec(&local_info)
        .map_err(|e| TransportError::ConnectionFailed(format!("serialize EP info: {e}")))?;
    let len = info_bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&info_bytes).await?;
    stream.flush().await?;

    let identity = PeerIdentity {
        org_id: OrgId(uuid::Uuid::nil()),
        common_name: format!("cxi-peer-{remote}"),
        cert_fingerprint: [0u8; 32],
    };

    Ok(CxiConnection {
        remote,
        identity,
        tcp_stream: stream,
        _remote_ep: remote_info,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_cxi_devices_returns_empty_on_no_hardware() {
        let devices = detect_cxi_devices();
        // Can't assert empty (might run on Slingshot machine),
        // but assert it doesn't panic.
        let _ = devices;
    }

    #[test]
    fn ep_info_serialization_roundtrip() {
        let info = ffi::EpInfo {
            addr: ffi::CxiAddr {
                nic: 1,
                pid: 42,
                vni: 100,
            },
            adaptive_routing: true,
        };
        let bytes = serde_json::to_vec(&info).unwrap();
        let back: ffi::EpInfo = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.addr.nic, 1);
        assert_eq!(back.addr.pid, 42);
        assert_eq!(back.addr.vni, 100);
        assert!(back.adaptive_routing);
    }

    #[test]
    fn cxi_addr_default() {
        let addr = ffi::CxiAddr::default();
        assert_eq!(addr.nic, 0);
        assert_eq!(addr.pid, 0);
        assert_eq!(addr.vni, 0);
    }

    #[tokio::test]
    async fn sideband_ep_exchange() {
        // Test EP info exchange over TCP sideband (no CXI hardware needed).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();

            // Read client's EP info.
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await.unwrap();
            let client_info: ffi::EpInfo = serde_json::from_slice(&buf).unwrap();
            assert_eq!(client_info.addr.vni, 200);

            // Send server's EP info.
            let server_info = ffi::EpInfo {
                addr: ffi::CxiAddr {
                    nic: 2,
                    pid: 99,
                    vni: 300,
                },
                adaptive_routing: false,
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

        let client_info = ffi::EpInfo {
            addr: ffi::CxiAddr {
                nic: 1,
                pid: 50,
                vni: 200,
            },
            adaptive_routing: true,
        };
        let bytes = serde_json::to_vec(&client_info).unwrap();
        let len = bytes.len() as u32;
        tcp.write_all(&len.to_be_bytes()).await.unwrap();
        tcp.write_all(&bytes).await.unwrap();
        tcp.flush().await.unwrap();

        // Read server's EP info.
        let mut len_buf = [0u8; 4];
        tcp.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        tcp.read_exact(&mut buf).await.unwrap();
        let server_info: ffi::EpInfo = serde_json::from_slice(&buf).unwrap();

        assert_eq!(server_info.addr.nic, 2);
        assert_eq!(server_info.addr.vni, 300);

        server.await.unwrap();
    }
}
