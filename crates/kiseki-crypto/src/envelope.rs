//! Envelope encryption/decryption and tenant KEK wrapping.
//!
//! An [`Envelope`] carries encrypted chunk data plus all metadata needed
//! to decrypt it:
//! - `ciphertext` + `auth_tag` + `nonce` (system-layer AEAD)
//! - `system_epoch` (which master key was used)
//! - `tenant_wrapped_material` (tenant KEK wraps the derivation params)
//!
//! Spec: `ubiquitous-language.md#Envelope`, ADR-003, I-K1, I-K3, I-K7.

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use zeroize::Zeroizing;

use crate::aead::{self, Aead, GCM_NONCE_LEN, GCM_TAG_LEN};
use crate::error::CryptoError;
use crate::hkdf::derive_system_dek;
use crate::keys::{SystemMasterKey, TenantKek};

/// Complete envelope for an encrypted chunk or delta payload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    /// Encrypted data (without tag — tag is separate for clarity).
    pub ciphertext: Vec<u8>,
    /// AEAD authentication tag (16 bytes).
    pub auth_tag: [u8; GCM_TAG_LEN],
    /// Nonce used for encryption (12 bytes).
    pub nonce: [u8; GCM_NONCE_LEN],
    /// System key epoch used for DEK derivation.
    pub system_epoch: KeyEpoch,
    /// Tenant key epoch used for wrapping (set after `wrap_for_tenant`).
    pub tenant_epoch: Option<KeyEpoch>,
    /// Tenant KEK wraps the system DEK derivation material.
    /// Set after `wrap_for_tenant`. Without this, only the system
    /// (with master key) can decrypt.
    pub tenant_wrapped_material: Option<Vec<u8>>,
    /// Chunk ID — in clear for dedup and routing.
    pub chunk_id: ChunkId,
}

/// Encrypt plaintext into an envelope using the system master key.
///
/// The chunk ID is used as HKDF salt (ADR-003) and as AAD for the AEAD.
pub fn seal_envelope(
    aead_ctx: &Aead,
    master: &SystemMasterKey,
    chunk_id: &ChunkId,
    plaintext: &[u8],
) -> Result<Envelope, CryptoError> {
    let dek = derive_system_dek(master, chunk_id)?;

    // AAD = chunk_id bytes — binds ciphertext to this specific chunk.
    let (ciphertext_with_tag, nonce) = aead_ctx.seal(&dek, plaintext, &chunk_id.0)?;

    // Split tag from ciphertext. aws-lc-rs appends the tag.
    let tag_start = ciphertext_with_tag.len() - GCM_TAG_LEN;
    let ciphertext = ciphertext_with_tag[..tag_start].to_vec();
    let mut auth_tag = [0u8; GCM_TAG_LEN];
    auth_tag.copy_from_slice(&ciphertext_with_tag[tag_start..]);

    Ok(Envelope {
        ciphertext,
        auth_tag,
        nonce,
        system_epoch: master.epoch,
        tenant_epoch: None,
        tenant_wrapped_material: None,
        chunk_id: *chunk_id,
    })
}

/// Decrypt an envelope using the system master key (system-layer only).
pub fn open_envelope(
    aead_ctx: &Aead,
    master: &SystemMasterKey,
    envelope: &Envelope,
) -> Result<Vec<u8>, CryptoError> {
    let dek = derive_system_dek(master, &envelope.chunk_id)?;

    // Reconstruct ciphertext+tag for aws-lc-rs.
    let mut ciphertext_with_tag = envelope.ciphertext.clone();
    ciphertext_with_tag.extend_from_slice(&envelope.auth_tag);

    aead_ctx.open(
        &dek,
        &envelope.nonce,
        &ciphertext_with_tag,
        &envelope.chunk_id.0,
    )
}

/// Wrap the system DEK derivation material with a tenant KEK so the
/// tenant can independently decrypt.
///
/// What we wrap: `(system_epoch || chunk_id)` — the tenant unwraps
/// this, then uses HKDF to re-derive the DEK. The actual DEK bytes
/// never leave the system boundary.
pub fn wrap_for_tenant(
    aead_ctx: &Aead,
    envelope: &mut Envelope,
    tenant_kek: &TenantKek,
) -> Result<(), CryptoError> {
    // Material to wrap: epoch (8 bytes) + chunk_id (32 bytes) = 40 bytes.
    let mut material = Vec::with_capacity(40);
    material.extend_from_slice(&envelope.system_epoch.0.to_le_bytes());
    material.extend_from_slice(&envelope.chunk_id.0);

    let key = Zeroizing::new(*tenant_kek.material());
    // AAD for the wrapping: "kiseki-tenant-wrap-v1" to distinguish from chunk AEAD.
    let (wrapped, nonce) = aead_ctx.seal(&key, &material, b"kiseki-tenant-wrap-v1")?;

    // Store nonce + wrapped material together.
    let mut combined = Vec::with_capacity(aead::GCM_NONCE_LEN + wrapped.len());
    combined.extend_from_slice(&nonce);
    combined.extend_from_slice(&wrapped);

    envelope.tenant_wrapped_material = Some(combined);
    envelope.tenant_epoch = Some(tenant_kek.epoch);

    Ok(())
}

/// Unwrap the tenant-wrapped material, re-derive the system DEK, and
/// decrypt the envelope. This is the tenant read path.
pub fn unwrap_tenant(
    aead_ctx: &Aead,
    envelope: &Envelope,
    tenant_kek: &TenantKek,
    master_cache: &crate::keys::MasterKeyCache,
) -> Result<Vec<u8>, CryptoError> {
    let wrapped = envelope
        .tenant_wrapped_material
        .as_ref()
        .ok_or_else(|| CryptoError::InvalidEnvelope("no tenant wrapping".into()))?;

    if wrapped.len() < aead::GCM_NONCE_LEN {
        return Err(CryptoError::InvalidEnvelope(
            "wrapped material too short".into(),
        ));
    }

    // Split nonce from wrapped ciphertext.
    let mut nonce = [0u8; aead::GCM_NONCE_LEN];
    nonce.copy_from_slice(&wrapped[..aead::GCM_NONCE_LEN]);
    let wrapped_ct = &wrapped[aead::GCM_NONCE_LEN..];

    let key = Zeroizing::new(*tenant_kek.material());
    let material = aead_ctx.open(&key, &nonce, wrapped_ct, b"kiseki-tenant-wrap-v1")?;

    // Parse unwrapped material: epoch (8 bytes) + chunk_id (32 bytes).
    if material.len() != 40 {
        return Err(CryptoError::InvalidEnvelope(format!(
            "unwrapped material length {}, expected 40",
            material.len()
        )));
    }
    let epoch_bytes: [u8; 8] = material[..8]
        .try_into()
        .map_err(|_| CryptoError::InvalidEnvelope("epoch parse failed".into()))?;
    let epoch = KeyEpoch(u64::from_le_bytes(epoch_bytes));

    // Defense-in-depth: verify unwrapped chunk_id matches the envelope
    // (ADV-PHASE1-005). The AEAD auth tag would catch a mismatch anyway,
    // but an explicit check gives a clearer error.
    let mut unwrapped_chunk_id = [0u8; 32];
    unwrapped_chunk_id.copy_from_slice(&material[8..40]);
    if unwrapped_chunk_id != envelope.chunk_id.0 {
        return Err(CryptoError::InvalidEnvelope(
            "unwrapped chunk_id does not match envelope".into(),
        ));
    }

    // Look up the master key for this epoch.
    let master = master_cache
        .get(epoch)
        .ok_or(CryptoError::TenantKeyFailed)?;

    // Re-derive the system DEK and decrypt.
    open_envelope(aead_ctx, master, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MasterKeyCache;

    fn test_master() -> SystemMasterKey {
        SystemMasterKey::new([0x42; 32], KeyEpoch(1))
    }

    fn test_tenant_kek() -> TenantKek {
        TenantKek::new([0xaa; 32], KeyEpoch(1))
    }

    fn test_chunk_id() -> ChunkId {
        ChunkId([0xbb; 32])
    }

    #[test]
    fn seal_open_roundtrip() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();
        let plaintext = b"hello kiseki";

        let envelope = seal_envelope(&aead, &master, &chunk_id, plaintext);
        assert!(envelope.is_ok());
        let envelope = envelope.unwrap_or_else(|_| unreachable!());

        let decrypted = open_envelope(&aead, &master, &envelope);
        assert!(decrypted.is_ok());
        assert_eq!(decrypted.unwrap_or_else(|_| unreachable!()), plaintext);
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let aead = Aead::new();
        let master = test_master();
        let wrong_master = SystemMasterKey::new([0x99; 32], KeyEpoch(1));
        let chunk_id = test_chunk_id();

        let envelope =
            seal_envelope(&aead, &master, &chunk_id, b"secret").unwrap_or_else(|_| unreachable!());
        let result = open_envelope(&aead, &wrong_master, &envelope);
        assert!(result.is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();

        let mut envelope =
            seal_envelope(&aead, &master, &chunk_id, b"data").unwrap_or_else(|_| unreachable!());
        if let Some(byte) = envelope.ciphertext.first_mut() {
            *byte ^= 0xff;
        }
        let result = open_envelope(&aead, &master, &envelope);
        assert!(result.is_err());
    }

    #[test]
    fn tenant_wrap_unwrap_roundtrip() {
        let aead = Aead::new();
        let master = test_master();
        let tenant_kek = test_tenant_kek();
        let chunk_id = test_chunk_id();
        let plaintext = b"tenant data";

        let mut envelope =
            seal_envelope(&aead, &master, &chunk_id, plaintext).unwrap_or_else(|_| unreachable!());
        wrap_for_tenant(&aead, &mut envelope, &tenant_kek).unwrap_or_else(|_| unreachable!());

        assert!(envelope.tenant_wrapped_material.is_some());
        assert_eq!(envelope.tenant_epoch, Some(KeyEpoch(1)));

        let mut cache = MasterKeyCache::new();
        cache.insert(SystemMasterKey::new([0x42; 32], KeyEpoch(1)));

        let decrypted = unwrap_tenant(&aead, &envelope, &tenant_kek, &cache);
        assert!(decrypted.is_ok());
        assert_eq!(decrypted.unwrap_or_else(|_| unreachable!()), plaintext);
    }

    #[test]
    fn wrong_tenant_kek_fails() {
        let aead = Aead::new();
        let master = test_master();
        let tenant_kek = test_tenant_kek();
        let wrong_kek = TenantKek::new([0xff; 32], KeyEpoch(1));
        let chunk_id = test_chunk_id();

        let mut envelope =
            seal_envelope(&aead, &master, &chunk_id, b"data").unwrap_or_else(|_| unreachable!());
        wrap_for_tenant(&aead, &mut envelope, &tenant_kek).unwrap_or_else(|_| unreachable!());

        let mut cache = MasterKeyCache::new();
        cache.insert(SystemMasterKey::new([0x42; 32], KeyEpoch(1)));

        let result = unwrap_tenant(&aead, &envelope, &wrong_kek, &cache);
        assert!(result.is_err());
    }

    #[test]
    fn missing_tenant_wrapping_fails() {
        let aead = Aead::new();
        let master = test_master();
        let tenant_kek = test_tenant_kek();
        let chunk_id = test_chunk_id();

        let envelope =
            seal_envelope(&aead, &master, &chunk_id, b"data").unwrap_or_else(|_| unreachable!());
        let cache = MasterKeyCache::new();

        let result = unwrap_tenant(&aead, &envelope, &tenant_kek, &cache);
        assert!(result.is_err());
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();

        let envelope =
            seal_envelope(&aead, &master, &chunk_id, b"").unwrap_or_else(|_| unreachable!());
        let decrypted = open_envelope(&aead, &master, &envelope).unwrap_or_else(|_| unreachable!());
        assert!(decrypted.is_empty());
    }

    #[test]
    fn tampered_auth_tag_rejected_ik7() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();

        let mut envelope = seal_envelope(&aead, &master, &chunk_id, b"secret data")
            .unwrap_or_else(|_| unreachable!());
        // Flip a byte in the auth tag.
        envelope.auth_tag[0] ^= 0xff;
        let result = open_envelope(&aead, &master, &envelope);
        assert!(result.is_err(), "tampered auth tag must be rejected (I-K7)");
    }

    #[test]
    fn nonce_uniqueness() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();
        let plaintext = b"same plaintext";

        let env1 =
            seal_envelope(&aead, &master, &chunk_id, plaintext).unwrap_or_else(|_| unreachable!());
        let env2 =
            seal_envelope(&aead, &master, &chunk_id, plaintext).unwrap_or_else(|_| unreachable!());

        // Two seals of the same plaintext must produce different nonces.
        assert_ne!(env1.nonce, env2.nonce, "nonces must differ between seals");
    }

    #[test]
    fn wrong_chunk_id_context_fails() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();
        let other_chunk = ChunkId([0xcc; 32]);

        let envelope = seal_envelope(&aead, &master, &chunk_id, b"bound to chunk")
            .unwrap_or_else(|_| unreachable!());

        // Reconstruct with wrong chunk_id — AAD mismatch should fail AEAD.
        let mut tampered_env = envelope.clone();
        tampered_env.chunk_id = other_chunk;
        let result = open_envelope(&aead, &master, &tampered_env);
        assert!(
            result.is_err(),
            "wrong chunk_id AAD must cause decryption failure"
        );
    }

    #[test]
    fn large_plaintext_roundtrip() {
        let aead = Aead::new();
        let master = test_master();
        let chunk_id = test_chunk_id();
        let plaintext = vec![0xab; 1024 * 1024]; // 1MB

        let envelope =
            seal_envelope(&aead, &master, &chunk_id, &plaintext).unwrap_or_else(|_| unreachable!());
        let decrypted = open_envelope(&aead, &master, &envelope).unwrap_or_else(|_| unreachable!());
        assert_eq!(decrypted, plaintext);
    }
}
