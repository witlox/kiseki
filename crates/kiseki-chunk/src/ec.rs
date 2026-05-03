//! Erasure coding — split chunks into data + parity fragments.
//!
//! Uses Reed-Solomon coding via `reed-solomon-erasure` crate.
//! Chunks survive up to `parity_shards` device failures.
//!
//! Spec: ADR-005, ADR-024, I-C4, I-D1, I-D4.

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::error::ChunkError;

/// EC-encoded chunk: a set of fragments (data + parity).
#[derive(Clone, Debug)]
pub struct EcEncoded {
    /// All fragments: data shards first, then parity shards.
    pub fragments: Vec<Vec<u8>>,
    /// Number of data shards.
    pub data_shards: usize,
    /// Number of parity shards.
    pub parity_shards: usize,
    /// Original data length before padding.
    pub original_len: usize,
}

/// Encode data into `data_shards` + `parity_shards` fragments.
///
/// Each fragment is `ceil(data.len() / data_shards)` bytes. The input
/// is zero-padded to be evenly divisible by `data_shards`.
///
/// Empty input is a valid degenerate case: returns `data + parity`
/// fragments of length 0 with `original_len = 0`. The RS encoder is
/// not invoked because there are no bytes to encode and the underlying
/// `reed-solomon-erasure` crate rejects zero-length shards. This case
/// arises in practice for POSIX `touch` (NFS OPEN+CREATE on an empty
/// file): the AEAD seal of a zero-byte plaintext yields zero ciphertext
/// bytes, and routing those through EC must succeed end-to-end.
#[tracing::instrument(skip(data), fields(bytes = data.len(), data_shards, parity_shards))]
pub fn encode(
    data: &[u8],
    data_shards: usize,
    parity_shards: usize,
) -> Result<EcEncoded, ChunkError> {
    if data_shards == 0 || parity_shards == 0 {
        tracing::warn!("ec encode: invalid config (shards must be non-zero)");
        return Err(ChunkError::EcInvalidConfig);
    }

    let original_len = data.len();

    // Empty-input degenerate case: emit `data + parity` zero-length
    // fragments. Skip the RS encoder — `reed-solomon-erasure` rejects
    // zero-length shards with `Error::EmptyShard`. Decode handles the
    // matching case: when `original_len == 0` it returns `Vec::new()`
    // without invoking the RS reconstructor. Round-trips with both
    // sides honoring this contract.
    if original_len == 0 {
        tracing::debug!("ec encode: empty input — returning zero-length fragments");
        let total = data_shards + parity_shards;
        return Ok(EcEncoded {
            fragments: vec![Vec::new(); total],
            data_shards,
            parity_shards,
            original_len: 0,
        });
    }

    let rs = ReedSolomon::new(data_shards, parity_shards).map_err(|e| {
        tracing::warn!(error = ?e, "ec encode: ReedSolomon::new failed");
        ChunkError::EcInvalidConfig
    })?;

    let shard_size = original_len.div_ceil(data_shards);

    // Build data shards with zero-padding.
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(data_shards + parity_shards);
    for i in 0..data_shards {
        let start = i * shard_size;
        let end = ((i + 1) * shard_size).min(original_len);
        let mut shard = if start < original_len {
            data[start..end].to_vec()
        } else {
            Vec::new()
        };
        shard.resize(shard_size, 0); // pad to uniform size
        shards.push(shard);
    }

    // Add empty parity shards.
    for _ in 0..parity_shards {
        shards.push(vec![0u8; shard_size]);
    }

    // Compute parity.
    rs.encode(&mut shards).map_err(|e| {
        tracing::warn!(error = ?e, original_len, shard_size, "ec encode: RS encode failed");
        ChunkError::EcEncodeFailed
    })?;

    tracing::debug!(
        original_len,
        shard_size,
        fragments = shards.len(),
        "ec encode: success",
    );
    Ok(EcEncoded {
        fragments: shards,
        data_shards,
        parity_shards,
        original_len,
    })
}

/// Decode fragments back to original data.
///
/// `fragments` must have `data_shards + parity_shards` entries.
/// Missing fragments should be `None`. At least `data_shards` fragments
/// must be present for reconstruction.
///
/// Empty-original case: when `original_len == 0` the function returns
/// `Vec::new()` without invoking the RS reconstructor, mirroring the
/// `encode` short-circuit. The fragment slice may contain `None`
/// entries (a node that doesn't have the empty fragment locally) or
/// `Some(Vec::new())` entries — both are fine because there's nothing
/// to reconstruct.
#[tracing::instrument(skip(fragments), fields(data_shards, parity_shards, original_len))]
pub fn decode(
    fragments: &mut [Option<Vec<u8>>],
    data_shards: usize,
    parity_shards: usize,
    original_len: usize,
) -> Result<Vec<u8>, ChunkError> {
    let total = data_shards + parity_shards;
    if fragments.len() != total {
        tracing::warn!(
            got = fragments.len(),
            want = total,
            "ec decode: invalid fragment count",
        );
        return Err(ChunkError::EcInvalidConfig);
    }

    // Empty-original short-circuit (mirrors encode). The plaintext is
    // zero bytes regardless of which fragments are present; skip the
    // RS reconstruct call.
    if original_len == 0 {
        tracing::debug!("ec decode: original_len == 0 — returning empty");
        return Ok(Vec::new());
    }

    let present = fragments.iter().filter(|f| f.is_some()).count();
    if present < data_shards {
        tracing::warn!(
            present,
            required = data_shards,
            "ec decode: insufficient fragments",
        );
        return Err(ChunkError::ChunkLost);
    }

    let rs = ReedSolomon::new(data_shards, parity_shards).map_err(|e| {
        tracing::warn!(error = ?e, "ec decode: ReedSolomon::new failed");
        ChunkError::EcInvalidConfig
    })?;

    // Reconstruct missing shards.
    rs.reconstruct(fragments).map_err(|e| {
        tracing::warn!(error = ?e, "ec decode: RS reconstruct failed");
        ChunkError::ChunkLost
    })?;

    // Reassemble from data shards.
    let mut result = Vec::with_capacity(original_len);
    for frag in fragments.iter().take(data_shards).flatten() {
        result.extend_from_slice(frag);
    }
    result.truncate(original_len);

    tracing::debug!(returned_bytes = result.len(), "ec decode: success");
    Ok(result)
}

/// Compute storage overhead ratio for an EC scheme.
#[must_use]
pub fn overhead_ratio(data_shards: usize, parity_shards: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        (data_shards + parity_shards) as f64 / data_shards as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip_4_2() {
        let data = vec![0xab; 1024 * 1024]; // 1MB
        let encoded = encode(&data, 4, 2).unwrap();

        assert_eq!(encoded.fragments.len(), 6);
        assert_eq!(encoded.data_shards, 4);
        assert_eq!(encoded.parity_shards, 2);

        // Decode with all fragments present.
        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 4, 2, data.len()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn encode_decode_roundtrip_8_3() {
        let data = vec![0xcd; 4 * 1024 * 1024]; // 4MB
        let encoded = encode(&data, 8, 3).unwrap();
        assert_eq!(encoded.fragments.len(), 11);

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 8, 3, data.len()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn degraded_decode_one_missing() {
        let data = vec![0xab; 1024 * 1024];
        let encoded = encode(&data, 4, 2).unwrap();

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        frags[2] = None; // device d3 offline

        let decoded = decode(&mut frags, 4, 2, data.len()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn degraded_decode_two_missing() {
        let data = vec![0xab; 1024 * 1024];
        let encoded = encode(&data, 4, 2).unwrap();

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        frags[2] = None; // d3 offline
        frags[4] = None; // d5 offline

        let decoded = decode(&mut frags, 4, 2, data.len()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn too_many_missing_fails() {
        let data = vec![0xab; 1024 * 1024];
        let encoded = encode(&data, 4, 2).unwrap();

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        frags[2] = None; // 3 missing > parity count 2
        frags[4] = None;
        frags[5] = None;

        let result = decode(&mut frags, 4, 2, data.len());
        assert!(result.is_err());
    }

    #[test]
    fn small_chunk_ec() {
        let data = vec![0x42; 4096]; // 4KB
        let encoded = encode(&data, 4, 2).unwrap();
        assert_eq!(encoded.fragments.len(), 6);
        // Each fragment is 1KB.
        assert_eq!(encoded.fragments[0].len(), 1024);

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 4, 2, data.len()).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn encode_decode_empty_input_4_2() {
        // Regression for the GCP 2026-05-02 NFSv4 OPEN(CREATE) → EIO
        // failure. NFS `touch` writes a 0-byte payload; routing it
        // through EC must not error, must round-trip cleanly to
        // empty bytes, and must produce the right fragment count.
        let data: Vec<u8> = Vec::new();
        let encoded = encode(&data, 4, 2).unwrap();

        assert_eq!(encoded.fragments.len(), 6);
        assert_eq!(encoded.original_len, 0);
        for f in &encoded.fragments {
            assert!(f.is_empty(), "empty input should produce empty fragments");
        }

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 4, 2, 0).unwrap();
        assert!(decoded.is_empty(), "round-trip of empty must yield empty");
    }

    #[test]
    fn encode_decode_empty_input_8_3() {
        let encoded = encode(&[], 8, 3).unwrap();
        assert_eq!(encoded.fragments.len(), 11);
        assert_eq!(encoded.original_len, 0);

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 8, 3, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_empty_with_some_fragments_missing() {
        // Even when fragments are missing, an empty-original decode
        // must succeed: there's nothing to reconstruct.
        let mut frags: Vec<Option<Vec<u8>>> = vec![None, None, None, None, None, None];
        let decoded = decode(&mut frags, 4, 2, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn encode_one_byte_4_2() {
        // Single byte: shard_size = ceil(1/4) = 1, so each data shard
        // is 1 byte (data[0..1] for shard 0, padded zero for 1..=3).
        let data = vec![0x77u8];
        let encoded = encode(&data, 4, 2).unwrap();
        assert_eq!(encoded.fragments.len(), 6);
        assert_eq!(encoded.original_len, 1);
        for f in &encoded.fragments {
            assert_eq!(f.len(), 1);
        }
        assert_eq!(encoded.fragments[0][0], 0x77);

        let mut frags: Vec<Option<Vec<u8>>> =
            encoded.fragments.iter().map(|f| Some(f.clone())).collect();
        let decoded = decode(&mut frags, 4, 2, 1).unwrap();
        assert_eq!(decoded, vec![0x77u8]);
    }

    #[test]
    fn overhead_ratios() {
        let ratio_4_2 = overhead_ratio(4, 2);
        assert!((ratio_4_2 - 1.5).abs() < f64::EPSILON);

        let ratio_8_3 = overhead_ratio(8, 3);
        assert!((ratio_8_3 - 1.375).abs() < f64::EPSILON);
    }
}
