//! Client-side TCP-framed-postcard binding (ADR-042 §2.2).
//!
//! `TcpFramedClient` opens one TCP+rustls connection per `(client,
//! node)` and pipelines per-call requests on it via `request_id`
//! correlation. The reader task demultiplexes responses by
//! `request_id` and resolves per-call `oneshot` channels; the
//! writer side serializes frame writes through a mutex on the
//! `OwnedWriteHalf`.
//!
//! The client surface is binding-shaped (`call(verb_tag, payload)`)
//! rather than verb-shaped: the per-verb typed wrappers live in the
//! caller (`NativeClient`'s binding-selection layer) which encodes
//! the request body and decodes the response. Keeps this module
//! small and binding-agnostic above the codec boundary.

pub mod client;

pub use client::{TcpFramedClient, TcpFramedClientError};
