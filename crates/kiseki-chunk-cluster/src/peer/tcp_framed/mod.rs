// Module-level clippy allows: the wire/client/server modules are
// intentionally low-style — pattern-heavy decode loops and short
// match arms. Bigger-picture lints (correctness, complexity) still
// apply.
#![allow(
    clippy::single_match_else,
    clippy::doc_markdown,
    clippy::manual_let_else,
    clippy::match_same_arms
)]

//! Fabric `FabricPeer` over the TCP-framed-postcard wire (ADR-042
//! §2.2 applied to the inter-node fabric edge).
//!
//! Until 2026-06-01 the fabric ran exclusively over tonic gRPC. The
//! local 3-node loopback profile that day measured
//! `put_send.transport = 1 598 µs` per fragment under gRPC vs ~115 µs
//! of receiver-side work — the gRPC/h2 stack dominated. ADR-042 §2.2
//! moved the gateway↔client edge off gRPC for exactly this reason;
//! this module moves the fabric↔fabric edge too.
//!
//! Public surface:
//! - [`TcpFramedFabricPeer`] — client; implements [`crate::peer::FabricPeer`].
//! - [`TcpFramedFabricListener`] — server; spawns one task per accepted
//!   connection and dispatches verbs to the shared
//!   [`crate::server::ClusterChunkServer`] handler.
//! - [`wire`] — verb catalogue + typed request/response meta structs.

pub mod client;
pub mod server;
pub mod wire;

pub use client::TcpFramedFabricPeer;
pub use server::TcpFramedFabricListener;
