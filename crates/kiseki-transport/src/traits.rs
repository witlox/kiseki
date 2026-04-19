//! Transport abstraction trait.
//!
//! Every transport (TCP+TLS, CXI, RDMA verbs) implements [`Transport`].
//! Downstream crates depend on the trait, not the concrete type —
//! transport selection happens at composition time in `kiseki-server`.

use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;

use kiseki_common::ids::OrgId;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::TransportError;

/// Identity extracted from a peer's mTLS certificate or SPIFFE SVID.
///
/// Every authenticated connection yields a `PeerIdentity` that
/// downstream code uses for tenant-scoping (I-T1) and audit (I-A1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    /// Tenant organization extracted from the certificate's OU or
    /// SPIFFE URI (`spiffe://cluster/org/<org_id>`).
    pub org_id: OrgId,
    /// The certificate's Subject Common Name (or SPIFFE URI).
    pub common_name: String,
    /// SHA-256 fingerprint of the peer's leaf certificate.
    pub cert_fingerprint: [u8; 32],
}

/// A single authenticated, bidirectional byte-stream connection.
///
/// Wraps an `AsyncRead + AsyncWrite` stream with the authenticated
/// peer identity. Dropping the connection closes the underlying stream.
pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {
    /// The authenticated identity of the remote peer.
    fn peer_identity(&self) -> &PeerIdentity;

    /// The remote socket address (for logging / metrics).
    fn remote_addr(&self) -> SocketAddr;
}

/// Pluggable transport abstraction.
///
/// Implementations must handle TLS termination, certificate validation,
/// and connection lifecycle. Uses associated types and RPITIT for
/// async methods.
pub trait Transport: Debug + Send + Sync + 'static {
    /// The concrete connection type returned by this transport.
    type Conn: Connection;

    /// Connect to a remote endpoint, perform the TLS handshake, and
    /// return an authenticated connection.
    fn connect(
        &self,
        addr: SocketAddr,
    ) -> impl Future<Output = Result<Self::Conn, TransportError>> + Send;

    /// A human-readable name for this transport (e.g., `"tcp-tls"`, `"cxi"`, `"verbs"`).
    fn name(&self) -> &'static str;
}
