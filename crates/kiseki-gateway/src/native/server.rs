//! `ServerImpl` — tonic service wrapper over `GatewayOps`.
//!
//! Phase 2 of ADR-042. Body is built up TDD-style across subsequent
//! commits — this file currently exposes only the type skeleton so
//! downstream callers can pin the type.

use std::sync::Arc;

use crate::ops::GatewayOps;

use super::signing_keys::SigningKeys;

/// Native data-plane gRPC handler. Wraps a `GatewayOps` and signs
/// the various tokens (handle, DEK ticket, multipart) under the
/// shared [`SigningKeys`].
pub struct ServerImpl {
    ops: Arc<dyn GatewayOps>,
    signing_keys: Arc<SigningKeys>,
}

impl ServerImpl {
    /// Construct a new handler.
    #[must_use]
    pub fn new(ops: Arc<dyn GatewayOps>, signing_keys: Arc<SigningKeys>) -> Self {
        Self { ops, signing_keys }
    }

    /// Borrow the underlying `GatewayOps`. Used by tests.
    #[must_use]
    pub fn ops(&self) -> &Arc<dyn GatewayOps> {
        &self.ops
    }

    /// Borrow the signing-key store. Used by tests.
    #[must_use]
    pub fn signing_keys(&self) -> &Arc<SigningKeys> {
        &self.signing_keys
    }
}
