//! Native gRPC `GatewayDataService` (ADR-042).
//!
//! Layout:
//!
//! - [`signing_keys`] — HKDF-derived HMAC keys for the three native-side
//!   token kinds (handle, DEK-fetch, multipart). Multi-epoch with
//!   rotation grace.
//! - [`handle_token`] — POSIX file-handle tokens (§9, I-NG10).
//! - [`dek_fetch_ticket`] — TrustedCompute DEK-fetch tickets (§8).
//! - [`multipart_upload_id`] — self-describing multipart upload IDs
//!   (gate-1 round-2 N4).
//! - [`server`] — `ServerImpl` wrapping a `GatewayOps` behind the
//!   tonic-generated `GatewayDataService` trait.
//!
//! Phase 2 of the ADR-042 implementation plan
//! (`specs/implementation/adr-042-native-gateway.md`).

pub mod dek_fetch_ticket;
pub mod handle_token;
pub mod lease_store;
pub mod multipart_upload_id;
pub mod signing_keys;

pub mod server;

pub use server::ServerImpl;
pub use signing_keys::SigningKeys;
