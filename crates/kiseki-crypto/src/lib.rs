//! FIPS-validated cryptography for Kiseki.
//!
//! This crate provides:
//! - AES-256-GCM authenticated encryption via `aws-lc-rs` (I-K7)
//! - HKDF-SHA256 system DEK derivation per ADR-003
//! - Chunk ID derivation: SHA-256 (cross-tenant dedup) or HMAC-SHA256
//!   (tenant-isolated) per I-K10, I-X2
//! - Envelope encryption/decryption with tenant KEK wrapping
//! - Optional compress-then-encrypt with fixed-size padding (I-K14)
//! - `Zeroizing<T>` wrappers for all key material (I-K8)
//!
//! Invariant mapping:
//!   - I-K1  — `encrypt_chunk` is the only path to produce chunk ciphertext
//!   - I-K3  — delta payloads use the same AEAD path
//!   - I-K7  — AES-256-GCM only; no unauthenticated mode
//!   - I-K8  — all key material behind `Zeroizing`; excluded from Debug
//!   - I-K10 — `derive_chunk_id` dispatches on `DedupPolicy`
//!   - I-K14 — `compress_and_encrypt` gated on `compression` feature

// Allow unsafe only for mlock/madvise key-material memory protection.
// Every unsafe block has a SAFETY comment.
#![allow(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod aead;
pub mod chunk_id;
pub mod envelope;
pub mod error;
pub mod hkdf;
pub mod keys;
pub(crate) mod mem_protect;
pub mod shred;

#[cfg(feature = "compression")]
pub mod compress;

pub use aead::Aead;
pub use chunk_id::derive_chunk_id;
pub use envelope::{open_envelope, seal_envelope, unwrap_tenant, wrap_for_tenant};
pub use error::CryptoError;
pub use hkdf::derive_system_dek;
pub use keys::{MasterKeyCache, SystemMasterKey};
