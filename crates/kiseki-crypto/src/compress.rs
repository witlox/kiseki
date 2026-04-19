//! Compress-then-encrypt with fixed-size padding (I-K14).
//!
//! Default OFF (safest). Tenant opt-in. Compliance tags may prohibit.
//! CRIME/BREACH side-channel risk mitigated by padding to a fixed
//! alignment boundary.

use std::io::{Read as _, Write as _};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use kiseki_common::ids::ChunkId;

use crate::aead::Aead;
use crate::envelope::{seal_envelope, Envelope};
use crate::error::CryptoError;
use crate::keys::SystemMasterKey;

/// Compress plaintext, pad to `pad_alignment`, then encrypt.
///
/// Padding ensures the ciphertext length is a multiple of
/// `pad_alignment`, which prevents CRIME/BREACH-style compression
/// ratio attacks from leaking information about the plaintext.
pub fn compress_and_encrypt(
    aead_ctx: &Aead,
    master: &SystemMasterKey,
    chunk_id: &ChunkId,
    plaintext: &[u8],
    pad_alignment: usize,
) -> Result<Envelope, CryptoError> {
    if pad_alignment == 0 {
        return Err(CryptoError::CompressionFailed(
            "pad alignment must be > 0".into(),
        ));
    }

    // Compress.
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(plaintext)
        .map_err(|e| CryptoError::CompressionFailed(e.to_string()))?;
    let mut compressed = encoder
        .finish()
        .map_err(|e| CryptoError::CompressionFailed(e.to_string()))?;

    // Pad to alignment boundary.
    let padded_len = compressed
        .len()
        .checked_next_multiple_of(pad_alignment)
        .unwrap_or(compressed.len());
    compressed.resize(padded_len, 0);

    // Encrypt the padded compressed data.
    seal_envelope(aead_ctx, master, chunk_id, &compressed)
}

/// Decrypt and decompress.
pub fn decrypt_and_decompress(
    aead_ctx: &Aead,
    master: &SystemMasterKey,
    envelope: &Envelope,
) -> Result<Vec<u8>, CryptoError> {
    let compressed_padded = crate::envelope::open_envelope(aead_ctx, master, envelope)?;

    // Decompress (deflate ignores trailing padding).
    let mut decoder = DeflateDecoder::new(&compressed_padded[..]);
    let mut plaintext = Vec::new();
    decoder
        .read_to_end(&mut plaintext)
        .map_err(|e| CryptoError::CompressionFailed(e.to_string()))?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::tenancy::KeyEpoch;

    #[test]
    fn compress_encrypt_decrypt_decompress_roundtrip() {
        let aead = Aead::new();
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xcc; 32]);
        let plaintext = b"hello world, this is some compressible data data data data";

        let envelope = compress_and_encrypt(&aead, &master, &chunk_id, plaintext, 256);
        assert!(envelope.is_ok());
        let envelope = envelope.unwrap_or_else(|_| unreachable!());

        // Ciphertext length should be a multiple of 256 (plus GCM tag overhead
        // is in auth_tag, not ciphertext).
        assert_eq!(envelope.ciphertext.len() % 256, 0);

        let decrypted = decrypt_and_decompress(&aead, &master, &envelope);
        assert!(decrypted.is_ok());
        assert_eq!(
            decrypted.unwrap_or_else(|_| unreachable!()).as_slice(),
            plaintext
        );
    }

    #[test]
    fn zero_pad_alignment_rejected() {
        let aead = Aead::new();
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xcc; 32]);
        let result = compress_and_encrypt(&aead, &master, &chunk_id, b"data", 0);
        assert!(result.is_err());
    }
}
