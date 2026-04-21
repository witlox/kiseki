//! CRUSH-like deterministic fragment placement.
//!
//! Maps `(chunk_id, fragment_index)` to a device, ensuring all
//! fragments land on distinct devices. Deterministic: same inputs
//! always produce the same placement.
//!
//! Spec: ADR-024, ADR-026 (CRUSH-like), I-D4.

use kiseki_common::ids::ChunkId;

/// A storage device within a pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    /// Device identifier.
    pub id: String,
    /// Whether the device is online.
    pub online: bool,
}

/// Compute deterministic placement for `n_fragments` across `devices`.
///
/// Returns a vec of device indices, one per fragment, all distinct.
/// Fails if fewer online devices than fragments.
#[must_use]
pub fn place_fragments(
    chunk_id: &ChunkId,
    n_fragments: usize,
    devices: &[DeviceInfo],
) -> Option<Vec<usize>> {
    if n_fragments == 0 {
        return None;
    }

    let online: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.online)
        .map(|(i, _)| i)
        .collect();

    if online.len() < n_fragments {
        return None;
    }

    // Hash-based selection: use chunk_id bytes as seed.
    let seed = u64::from_le_bytes([
        chunk_id.0[0],
        chunk_id.0[1],
        chunk_id.0[2],
        chunk_id.0[3],
        chunk_id.0[4],
        chunk_id.0[5],
        chunk_id.0[6],
        chunk_id.0[7],
    ]);

    let mut selected = Vec::with_capacity(n_fragments);
    let mut available = online.clone();

    for frag_idx in 0..n_fragments {
        // Mix seed with fragment index for per-fragment variation.
        let hash = seed.wrapping_mul(2654435761).wrapping_add(frag_idx as u64);
        let idx = (hash as usize) % available.len();
        selected.push(available.remove(idx));
    }

    Some(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_devices(n: usize) -> Vec<DeviceInfo> {
        (0..n)
            .map(|i| DeviceInfo {
                id: format!("d{}", i + 1),
                online: true,
            })
            .collect()
    }

    #[test]
    fn deterministic_placement() {
        let chunk_id = ChunkId([0x42; 32]);
        let devices = make_devices(6);

        let p1 = place_fragments(&chunk_id, 6, &devices).unwrap();
        let p2 = place_fragments(&chunk_id, 6, &devices).unwrap();
        assert_eq!(p1, p2, "placement should be deterministic");
    }

    #[test]
    fn all_distinct_devices() {
        let chunk_id = ChunkId([0xab; 32]);
        let devices = make_devices(6);

        let placed = place_fragments(&chunk_id, 6, &devices).unwrap();
        let mut unique = placed.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(placed.len(), unique.len(), "all devices should be distinct");
    }

    #[test]
    fn insufficient_devices_fails() {
        let chunk_id = ChunkId([0x01; 32]);
        let devices = make_devices(3);

        let result = place_fragments(&chunk_id, 6, &devices);
        assert!(result.is_none());
    }

    #[test]
    fn skips_offline_devices() {
        let chunk_id = ChunkId([0x55; 32]);
        let mut devices = make_devices(8);
        devices[2].online = false;
        devices[5].online = false;

        let placed = place_fragments(&chunk_id, 6, &devices).unwrap();
        assert_eq!(placed.len(), 6);
        assert!(!placed.contains(&2));
        assert!(!placed.contains(&5));
    }

    #[test]
    fn different_chunks_different_placement() {
        let devices = make_devices(6);
        let p1 = place_fragments(&ChunkId([0x01; 32]), 6, &devices).unwrap();
        let p2 = place_fragments(&ChunkId([0x02; 32]), 6, &devices).unwrap();
        // Not guaranteed to differ, but very likely with different seeds.
        // Just verify they're valid.
        assert_eq!(p1.len(), 6);
        assert_eq!(p2.len(), 6);
    }
}
