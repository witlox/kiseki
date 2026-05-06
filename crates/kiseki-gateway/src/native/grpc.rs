//! gRPC binding for the native gateway data service (ADR-042 §2.1).
//!
//! This module hosts the gRPC-binding-specific adapters that mediate
//! between tonic's request-shaped types (`tonic::Request<T>`,
//! `tonic::Status`, request extensions) and the binding-agnostic
//! contract surface in [`kiseki_proto::native_contract`]
//! (`RequestPrincipal`, `NativeError`).
//!
//! Phase 1 of the post-2026-05-06 redesign per
//! `specs/architecture/adr/042-native-gateway-data-service.md` §16.1.
//!
//! Layout:
//! - [`principal`] — `TonicPrincipal` adapter implementing
//!   `RequestPrincipal` over a tonic `Request<T>`.
//! - (forthcoming) `adapter` — `Arc<NativeHandler>` → tonic
//!   `GatewayDataService` shim moving binding code out of
//!   `kiseki-gateway::native::server` per §1.8 enforcement rule.

pub mod adapter;
pub mod principal;
pub mod probe;

pub use adapter::GrpcAdapter;
pub use principal::{principal_from_request, TonicPrincipal};
pub use probe::GrpcProbe;
