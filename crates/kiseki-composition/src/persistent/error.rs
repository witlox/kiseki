//! Typed errors for the persistent composition store (ADR-040 §D8.1).
//!
//! Maps to `GatewayError::Upstream(string)` at the gateway boundary —
//! no `GatewayError` taxonomy change. The discriminator surfaces in
//! `kiseki_composition_decode_errors_total{kind=...}` (§D10) so
//! operators can route alarms by error kind.

use crate::error::CompositionError;

/// Errors raised by the persistent composition storage layer.
#[derive(Debug, thiserror::Error)]
pub enum PersistentStoreError {
    /// I/O against the underlying backend (open, read, write, fsync).
    #[error("persistent store I/O: {0}")]
    Io(#[from] std::io::Error),

    /// The on-disk record carries a `schema_version` this binary
    /// doesn't know how to decode. Surfaced as "binary too old".
    #[error("persistent store schema too new: found={found} supported={supported}")]
    SchemaTooNew {
        /// The `schema_version` byte read from disk.
        found: u8,
        /// The highest `schema_version` this binary can decode.
        supported: u8,
    },

    /// Postcard decode failure — payload bytes don't match the
    /// declared schema's struct shape.
    #[error("persistent store decode: {0}")]
    Decode(String),

    /// A persistent-store call delegated to an in-memory
    /// `CompositionStore` operation (e.g. `create_at` rule
    /// validation) and that returned a domain error.
    #[error("persistent store domain error: {0}")]
    Composition(#[from] CompositionError),

    /// Backend commit / persist failed — surfaced separately from raw
    /// I/O so operators can distinguish "fsync of in-flight batch
    /// failed" from "open / read / static-init failed".
    #[error("persistent store commit: {0}")]
    Commit(String),

    /// Catch-all for backend-specific table / batch errors that don't
    /// map to a more precise variant.
    #[error("persistent store backend: {0}")]
    Backend(String),

    /// Underlying fjall LSM error (open, get, put, batch, persist).
    /// Preserves the source error so operators see the full chain;
    /// `metric_kind()` reports `"fjall"` for the
    /// `kiseki_composition_decode_errors_total` metric.
    #[error("persistent store fjall: {0}")]
    Fjall(#[from] fjall::Error),
}

impl PersistentStoreError {
    /// Discriminator label for `kiseki_composition_decode_errors_total`
    /// metric `kind=` field.
    #[must_use]
    pub fn metric_kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::SchemaTooNew { .. } => "schema_too_new",
            Self::Decode(_) => "decode",
            Self::Composition(_) => "composition",
            Self::Commit(_) => "commit",
            Self::Backend(_) => "backend",
            Self::Fjall(_) => "fjall",
        }
    }
}

impl From<postcard::Error> for PersistentStoreError {
    fn from(e: postcard::Error) -> Self {
        Self::Decode(e.to_string())
    }
}
