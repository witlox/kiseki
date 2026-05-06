//! TCP-framed-postcard binding for the native gateway data service
//! (ADR-042 §2.2).
//!
//! This module hosts the TCP-framed binding's adapters that mediate
//! between the wire shape (postcard envelope over length-framed
//! rustls/TCP) and the binding-agnostic contract surface in
//! [`kiseki_proto::native_contract`] (`RequestPrincipal`,
//! `NativeError`).
//!
//! Layout:
//! - [`principal`] — `TcpFramedPrincipal` adapter implementing
//!   `RequestPrincipal` over a per-connection canonical-SAN stash
//!   established at the rustls handshake.
//! - (forthcoming slices) `dispatch` — verb-tag → `ServerImpl`
//!   inherent method dispatch table; `listener` — TCP+rustls
//!   listener with per-connection accept loop + frame loop;
//!   `client` — connection establishment + multiplexed
//!   request/response.

pub mod connection;
pub mod dispatch;
pub mod listener;
pub mod principal;
pub mod probe;

pub use connection::{serve_connection, ConnectionError};
pub use dispatch::{dispatch_verb, status_to_wire};
pub use listener::{TcpFramedListener, NATIVE_TCP_FRAMED_PER_PEER_MAX};
pub use principal::TcpFramedPrincipal;
pub use probe::TcpFramedProbe;
