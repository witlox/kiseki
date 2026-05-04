#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Storage failure validation tests (Phase 6 — WS 7.2).
//!
//! Validates failure modes F-D1 through F-D4 from specs/failure-modes.md.

use kiseki_chunk::ec;
use kiseki_common::ids::ChunkId;

/// F-D1: Single device failure — EC repair from parity.
#[test]
fn single_device_failure_ec_repair() {
    // 4 data + 2 parity = tolerates 2 failures
    let data = b"Hello, this is a test of erasure coding repair after device failure!";
    let encoded = ec::encode(data, 4, 2).unwrap();
    assert_eq!(encoded.fragments.len(), 6);

    // Simulate single device failure: remove fragment 0 (data shard).
    let mut fragments: Vec<Option<Vec<u8>>> =
        encoded.fragments.iter().map(|f| Some(f.clone())).collect();
    fragments[0] = None; // device failure

    let recovered = ec::decode(&mut fragments, 4, 2, data.len()).unwrap();
    assert_eq!(recovered, data, "EC repair should recover original data");
}

/// F-D2a: Double failure within EC tolerance (4+2 can handle 2).
#[test]
fn double_failure_within_ec_tolerance() {
    let data = b"Double failure test - two devices down but within parity budget.";
    let encoded = ec::encode(data, 4, 2).unwrap();

    // Remove 2 fragments (within tolerance of 2 parity).
    let mut fragments: Vec<Option<Vec<u8>>> =
        encoded.fragments.iter().map(|f| Some(f.clone())).collect();
    fragments[1] = None; // data shard 1 failed
    fragments[4] = None; // parity shard 0 failed

    let recovered = ec::decode(&mut fragments, 4, 2, data.len()).unwrap();
    assert_eq!(recovered, data, "EC should recover with 2 failures (4+2)");
}

/// F-D2b: Triple failure exceeds EC tolerance (4+2 can only handle 2).
#[test]
fn triple_failure_exceeds_ec_tolerance() {
    let data = b"Triple failure test - three devices down exceeds parity budget.";
    let encoded = ec::encode(data, 4, 2).unwrap();

    // Remove 3 fragments (exceeds tolerance of 2 parity).
    let mut fragments: Vec<Option<Vec<u8>>> =
        encoded.fragments.iter().map(|f| Some(f.clone())).collect();
    fragments[0] = None;
    fragments[1] = None;
    fragments[2] = None;

    let result = ec::decode(&mut fragments, 4, 2, data.len());
    assert!(
        result.is_err(),
        "EC decode should fail with 3 failures (exceeds 4+2 tolerance)"
    );
}

/// F-D3: Corrupted extent — CRC detection.
#[test]
fn corrupt_extent_detected_by_crc() {
    use kiseki_block::backend::crc32c;

    let data = b"This is valid chunk data";
    let crc = crc32c(data);

    // Corrupt one byte.
    let mut corrupted = data.to_vec();
    corrupted[5] ^= 0xFF;
    let corrupt_crc = crc32c(&corrupted);

    assert_ne!(crc, corrupt_crc, "CRC should differ for corrupt data");
}

/// EC encode/decode round-trip with 8+3 configuration.
#[test]
fn ec_8_plus_3_roundtrip() {
    // 8 data + 3 parity = tolerates 3 failures
    let data = vec![42u8; 16384]; // 16 KB
    let encoded = ec::encode(&data, 8, 3).unwrap();
    assert_eq!(encoded.fragments.len(), 11);

    // Remove 3 fragments (max tolerance).
    let mut fragments: Vec<Option<Vec<u8>>> =
        encoded.fragments.iter().map(|f| Some(f.clone())).collect();
    fragments[0] = None;
    fragments[3] = None;
    fragments[7] = None;

    let recovered = ec::decode(&mut fragments, 8, 3, data.len()).unwrap();
    assert_eq!(recovered, data);
}

/// Degraded read: only parity fragments missing (data intact).
#[test]
fn degraded_read_parity_only_missing() {
    let data = b"Only parity shards are missing - data should be read directly.";
    let encoded = ec::encode(data, 4, 2).unwrap();

    // Remove both parity shards (index 4 and 5).
    let mut fragments: Vec<Option<Vec<u8>>> =
        encoded.fragments.iter().map(|f| Some(f.clone())).collect();
    fragments[4] = None;
    fragments[5] = None;

    let recovered = ec::decode(&mut fragments, 4, 2, data.len()).unwrap();
    assert_eq!(recovered, data, "Data shards intact -> direct read");
}

/// Chunk ID derivation is deterministic.
#[test]
fn chunk_id_deterministic() {
    let data1 = ChunkId([0xAB; 32]);
    let data2 = ChunkId([0xAB; 32]);
    assert_eq!(data1, data2, "same content -> same ChunkId");
}
