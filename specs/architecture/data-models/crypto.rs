//! Cryptographic types — FIPS AEAD, envelope encryption, key wrapping.
//! Spec: invariants.md#Encryption, ubiquitous-language.md#Encryption
//! ADR: 003-system-dek-derivation

use crate::common::*;

// --- Algorithm identifiers (crypto-agility, spec: A-R2) ---

/// Algorithm used for chunk/payload encryption.
pub enum EncryptionAlgorithm {
    Aes256Gcm,
    // Future: post-quantum candidates
}

/// Algorithm used for chunk ID derivation.
pub enum ChunkIdAlgorithm {
    Sha256,
    HmacSha256,
}

// --- System layer (always present) ---

/// System Data Encryption Key — encrypts chunk data.
/// Derived from system master key + chunk_id to avoid storing billions of DEKs.
/// Spec: ubiquitous-language.md#SystemDEK, ADR-003
pub struct SystemDek {
    /// Not stored — derived at runtime: HKDF(system_master_key, chunk_id, epoch)
    /// This struct represents the derived key in memory only.
    pub key_material: zeroize::Zeroizing<[u8; 32]>,
    pub epoch: KeyEpoch,
}

/// System Key Encryption Key — wraps/derives system DEKs.
/// Stored in the system key manager (HA service).
/// Spec: ubiquitous-language.md#SystemKEK
pub struct SystemKek {
    pub key_id: uuid::Uuid,
    pub epoch: KeyEpoch,
    /// Key material lives in the system key manager only.
    /// This struct is a reference, not the key itself.
}

/// System master key — the root of the system key hierarchy.
/// Stored in the system key manager's Raft-replicated state.
/// One per epoch. HKDF derives per-chunk DEKs.
pub struct SystemMasterKey {
    pub key_id: uuid::Uuid,
    pub epoch: KeyEpoch,
    pub algorithm: EncryptionAlgorithm,
    pub created_at: DeltaTimestamp,
}

// --- Tenant layer (wraps access to system layer) ---

/// Tenant Key Encryption Key — wraps system DEK for tenant access.
/// Destruction = crypto-shred.
/// Spec: ubiquitous-language.md#TenantKEK, I-K5
pub struct TenantKek {
    pub key_id: uuid::Uuid,
    pub org_id: OrgId,
    pub epoch: KeyEpoch,
    /// Obtained from tenant KMS — not stored in Kiseki.
    /// Cached with bounded TTL.
}

/// Cached tenant key material with TTL.
/// Spec: adversarial-findings.md#B-ADV-5
pub struct CachedTenantKey {
    pub kek: TenantKek,
    pub cached_at: WallTime,
    pub ttl_seconds: u32,
}

// --- Envelope ---

/// Complete envelope wrapping an encrypted chunk or delta payload.
/// Spec: ubiquitous-language.md#Envelope
pub struct Envelope {
    /// Encrypted data (ciphertext)
    pub ciphertext: Vec<u8>,
    /// AEAD authentication tag
    pub auth_tag: [u8; 16],
    /// Nonce/IV used for encryption
    pub nonce: [u8; 12],
    /// Algorithm identifier (crypto-agility)
    pub algorithm: EncryptionAlgorithm,
    /// System key epoch used for encryption
    pub system_epoch: KeyEpoch,
    /// Tenant key epoch used for wrapping (if tenant-scoped)
    pub tenant_epoch: Option<KeyEpoch>,
    /// Wrapped system DEK derivation material for tenant access
    /// (tenant KEK encrypts this; unwrapping yields the HKDF input)
    pub tenant_wrapped_material: Option<Vec<u8>>,
    /// Chunk ID (plaintext-derived, in clear for dedup)
    pub chunk_id: ChunkId,
}

// --- Operations (trait stubs, no bodies) ---

/// Encryption operations — implemented by kiseki-crypto.
pub trait CryptoOps {
    /// Encrypt plaintext, produce envelope.
    fn encrypt_chunk(
        &self,
        plaintext: &[u8],
        system_master: &SystemMasterKey,
        chunk_id: &ChunkId,
    ) -> Result<Envelope, KisekiError>;

    /// Decrypt chunk from envelope.
    fn decrypt_chunk(
        &self,
        envelope: &Envelope,
        system_master: &SystemMasterKey,
    ) -> Result<Vec<u8>, KisekiError>;

    /// Wrap system DEK derivation material with tenant KEK.
    fn wrap_for_tenant(
        &self,
        envelope: &mut Envelope,
        tenant_kek: &TenantKek,
    ) -> Result<(), KisekiError>;

    /// Unwrap system DEK using tenant KEK, then decrypt.
    fn decrypt_with_tenant_key(
        &self,
        envelope: &Envelope,
        tenant_kek: &TenantKek,
        system_master: &SystemMasterKey,
    ) -> Result<Vec<u8>, KisekiError>;

    /// Derive chunk ID from plaintext per tenant dedup policy.
    fn derive_chunk_id(
        &self,
        plaintext: &[u8],
        policy: &DedupPolicy,
        tenant_hmac_key: Option<&[u8]>,
    ) -> ChunkId;

    /// Compress-then-encrypt with padding (tenant opt-in).
    /// Spec: I-K14
    fn compress_and_encrypt(
        &self,
        plaintext: &[u8],
        system_master: &SystemMasterKey,
        chunk_id: &ChunkId,
        pad_alignment: usize,
    ) -> Result<Envelope, KisekiError>;
}
