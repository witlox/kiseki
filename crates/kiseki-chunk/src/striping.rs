//! Multi-device EC fragment striping.
//!
//! Distributes erasure-coded fragments across multiple devices using
//! deterministic CRUSH-like hashing. Enforces I-D4: no two fragments
//! of the same chunk may reside on the same device.
//!
//! Spec: ADR-005, ADR-024, I-D4.

use std::collections::HashSet;

use kiseki_common::ids::ChunkId;

/// A fragment-to-device assignment for an EC-encoded chunk.
#[derive(Clone, Debug)]
pub struct FragmentMap {
    /// Ordered list of fragment assignments (data first, then parity).
    pub fragments: Vec<FragmentAssignment>,
}

/// One fragment's placement on a specific device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentAssignment {
    /// Index within the EC fragment set (0-based).
    pub fragment_index: u32,
    /// Device UUID.
    pub device_id: [u8; 16],
    /// Byte offset on the device.
    pub offset: u64,
    /// Fragment length in bytes.
    pub length: u64,
    /// Whether this is a parity fragment.
    pub is_parity: bool,
}

/// Errors from fragment striping operations.
#[derive(Debug, thiserror::Error)]
pub enum StripingError {
    /// Fewer available devices than total fragments.
    #[error("insufficient devices: need {need}, have {have}")]
    InsufficientDevices {
        /// Number of devices required.
        need: usize,
        /// Number of devices available.
        have: usize,
    },

    /// A specific device is unavailable.
    #[error("device unavailable: {0:?}")]
    DeviceUnavailable([u8; 16]),
}

/// Deterministically assign EC fragments to devices using CRUSH-like hashing.
///
/// Each fragment is assigned to a distinct device (I-D4). The assignment is
/// deterministic: the same `chunk_id`, shard counts, and device list always
/// produce the same mapping.
///
/// # Errors
///
/// Returns `InsufficientDevices` if `available_devices.len() < data_count + parity_count`.
pub fn assign_fragments(
    chunk_id: &ChunkId,
    data_count: u32,
    parity_count: u32,
    available_devices: &[[u8; 16]],
) -> Result<FragmentMap, StripingError> {
    let total = (data_count + parity_count) as usize;

    if available_devices.len() < total {
        return Err(StripingError::InsufficientDevices {
            need: total,
            have: available_devices.len(),
        });
    }

    // CRUSH-like: score each device for each fragment, pick the highest-scoring
    // unused device. The score is a deterministic hash of (chunk_id, fragment_index, device_id).
    let mut used: HashSet<usize> = HashSet::with_capacity(total);
    let mut assignments = Vec::with_capacity(total);

    #[allow(clippy::cast_possible_truncation)]
    for frag_idx in 0..total as u32 {
        let mut best_score: u64 = 0;
        let mut best_dev: Option<usize> = None;

        for (dev_idx, device_id) in available_devices.iter().enumerate() {
            if used.contains(&dev_idx) {
                continue;
            }
            let score = crush_hash(&chunk_id.0, frag_idx, device_id);
            if best_dev.is_none() || score > best_score {
                best_score = score;
                best_dev = Some(dev_idx);
            }
        }

        // unwrap is safe: we verified available_devices.len() >= total above,
        // and used.len() < total at this point.
        let chosen = best_dev.expect("guaranteed by device count check");
        used.insert(chosen);

        assignments.push(FragmentAssignment {
            fragment_index: frag_idx,
            device_id: available_devices[chosen],
            offset: 0, // caller fills actual offset after allocation
            length: 0, // caller fills after knowing fragment size
            is_parity: frag_idx >= data_count,
        });
    }

    Ok(FragmentMap {
        fragments: assignments,
    })
}

/// CRUSH-like deterministic hash for (chunk, fragment, device) -> score.
///
/// Uses FNV-1a mixing for speed and good distribution.
fn crush_hash(chunk_id: &[u8; 32], fragment_index: u32, device_id: &[u8; 16]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis

    // Mix chunk ID.
    for &b in chunk_id {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3); // FNV-1a prime
    }

    // Mix fragment index.
    for &b in &fragment_index.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }

    // Mix device ID.
    for &b in device_id {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }

    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create N device UUIDs.
    #[allow(clippy::cast_possible_truncation)]
    fn make_devices(n: usize) -> Vec<[u8; 16]> {
        (0..n)
            .map(|i| {
                let mut id = [0u8; 16];
                id[0] = i as u8;
                id[15] = (i >> 8) as u8;
                id
            })
            .collect()
    }

    #[test]
    fn fragments_placed_on_distinct_devices() {
        let chunk_id = ChunkId([0xaa; 32]);
        let devices = make_devices(8);
        let map = assign_fragments(&chunk_id, 4, 2, &devices).unwrap();

        assert_eq!(map.fragments.len(), 6);

        // All device IDs must be unique (I-D4).
        let device_ids: HashSet<[u8; 16]> = map.fragments.iter().map(|f| f.device_id).collect();
        assert_eq!(device_ids.len(), 6);
    }

    #[test]
    fn insufficient_devices_error() {
        let chunk_id = ChunkId([0xbb; 32]);
        let devices = make_devices(3); // need 6, have 3

        let result = assign_fragments(&chunk_id, 4, 2, &devices);
        assert!(result.is_err());

        match result.unwrap_err() {
            StripingError::InsufficientDevices { need, have } => {
                assert_eq!(need, 6);
                assert_eq!(have, 3);
            }
            other @ StripingError::DeviceUnavailable(_) => {
                panic!("expected InsufficientDevices, got: {other}")
            }
        }
    }

    #[test]
    fn deterministic_assignment() {
        let chunk_id = ChunkId([0xcc; 32]);
        let devices = make_devices(10);

        let map1 = assign_fragments(&chunk_id, 4, 2, &devices).unwrap();
        let map2 = assign_fragments(&chunk_id, 4, 2, &devices).unwrap();

        for (a, b) in map1.fragments.iter().zip(map2.fragments.iter()) {
            assert_eq!(a.device_id, b.device_id);
            assert_eq!(a.fragment_index, b.fragment_index);
            assert_eq!(a.is_parity, b.is_parity);
        }
    }

    #[test]
    fn parity_fragments_correctly_tagged() {
        let chunk_id = ChunkId([0xdd; 32]);
        let devices = make_devices(8);
        let map = assign_fragments(&chunk_id, 4, 2, &devices).unwrap();

        for frag in &map.fragments {
            if frag.fragment_index < 4 {
                assert!(!frag.is_parity, "data fragment tagged as parity");
            } else {
                assert!(frag.is_parity, "parity fragment not tagged");
            }
        }
    }

    #[test]
    fn exact_device_count_succeeds() {
        let chunk_id = ChunkId([0xee; 32]);
        let devices = make_devices(6); // exactly 4+2

        let map = assign_fragments(&chunk_id, 4, 2, &devices).unwrap();
        assert_eq!(map.fragments.len(), 6);

        // Must use all devices.
        let device_ids: HashSet<[u8; 16]> = map.fragments.iter().map(|f| f.device_id).collect();
        assert_eq!(device_ids.len(), 6);
    }

    #[test]
    fn exact_device_count_equals_fragment_count_all_assigned() {
        // When device count == fragment count, every device must get
        // exactly one fragment (no device left unused).
        let chunk_id = ChunkId([0xff; 32]);
        let total_data = 3u32;
        let total_parity = 2u32;
        let total = (total_data + total_parity) as usize;
        let devices = make_devices(total);

        let map = assign_fragments(&chunk_id, total_data, total_parity, &devices).unwrap();
        assert_eq!(map.fragments.len(), total);

        // Every device must appear exactly once.
        let used_device_ids: HashSet<[u8; 16]> =
            map.fragments.iter().map(|f| f.device_id).collect();
        let all_device_ids: HashSet<[u8; 16]> = devices.iter().copied().collect();
        assert_eq!(used_device_ids, all_device_ids);
    }
}
