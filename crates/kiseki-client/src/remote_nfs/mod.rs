
//! NFS client library — `NFSv3`, `NFSv4.1`/`NFSv4.2`, and `pNFS` over TCP.
//!
//! Each version implements `GatewayOps` so BDD steps use the same
//! interface regardless of protocol version. ADR-023 D7.
//!
//! Shared ONC RPC transport lives here; version-specific COMPOUND
//! (v4) or procedure (v3) logic lives in submodules.

#![cfg(feature = "remote-nfs")]

pub mod transport;
pub mod v3;
pub mod v4;
