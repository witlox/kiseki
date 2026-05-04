//! System Key Manager for Kiseki.
//!
//! Manages system master keys across epochs. Storage nodes fetch the
//! current master key at startup and on rotation, then derive per-chunk
//! DEKs locally via HKDF (kiseki-crypto, ADR-003). The key manager
//! never sees individual chunk IDs.
//!
//! Invariant mapping:
//!   - I-K6  — rotation preserves old epoch keys during migration window
//!   - I-K8  — key material behind `Zeroizing`, excluded from Debug
//!   - I-K11 — tenant KMS loss = data loss (no escrow)
//!   - I-K12 — system key manager HA (Raft, ADR-007) — deferred to Raft integration

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cache;
pub mod epoch;
pub mod error;
pub mod grpc;
pub mod health;
pub mod internal_provider;
pub mod node_identity;
pub mod persistent_store;
pub mod provider;
pub mod raft;
pub mod raft_store;
pub mod rewrap_worker;
pub mod rotation_monitor;
pub mod store;

#[cfg(feature = "kms-aws")]
pub mod aws_provider;
#[cfg(feature = "kms-azure")]
pub mod azure_provider;
#[cfg(feature = "kms-gcp")]
pub mod gcp_provider;
#[cfg(feature = "kms-vault")]
pub mod vault_provider;

pub use epoch::{EpochInfo, KeyManagerOps};
pub use error::KeyManagerError;
pub use health::{KeyManagerHealth, KeyManagerStatus};
pub use internal_provider::InternalProvider;
pub use persistent_store::PersistentKeyStore;
pub use provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};
pub use raft::OpenRaftKeyStore;
pub use raft_store::RaftKeyStore;
pub use store::MemKeyStore;
