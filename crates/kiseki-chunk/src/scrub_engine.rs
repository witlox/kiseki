//! Device scrub engine.
//!
//! Performs background integrity checks on device storage: bitmap
//! consistency, CRC sampling, and orphan-extent detection.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

/// Configuration for a device scrub pass.
#[derive(Debug, Clone)]
pub struct ScrubConfig {
    /// Fraction of extents to CRC-check (0.0–1.0). Default 0.1.
    pub sample_rate: f64,
    /// Verify primary/mirror bitmap consistency. Default true.
    pub check_bitmap: bool,
    /// Detect allocated-but-unreferenced extents. Default true.
    pub check_orphans: bool,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            sample_rate: 0.1,
            check_bitmap: true,
            check_orphans: true,
        }
    }
}

/// Results of a device scrub pass.
#[derive(Debug, Clone)]
pub struct ScrubReport {
    /// Number of bitmap inconsistencies found.
    pub bitmap_errors: u64,
    /// Number of CRC check failures.
    pub crc_failures: u64,
    /// Number of allocated-but-unreferenced extents.
    pub orphan_extents: u64,
    /// Total extents checked during the scrub.
    pub total_extents_checked: u64,
    /// Wall-clock time for the scrub.
    pub elapsed: Duration,
    /// Device that was scrubbed.
    pub device_id: [u8; 16],
}

impl ScrubReport {
    /// Returns `true` when all error counts are zero.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.bitmap_errors == 0 && self.crc_failures == 0 && self.orphan_extents == 0
    }
}

/// Run a scrub pass over a device.
///
/// - `device_bitmap_bytes`: raw bitmap — each byte represents one extent
///   (non-zero = allocated).
/// - `mirror_bitmap_bytes`: mirror copy of the bitmap for consistency check.
/// - `chunk_ref_extents`: set of extent indices that are referenced by live
///   chunks.
/// - `crc_check`: callback `(extent_index) -> bool` that returns `true` when
///   the CRC for the given extent is valid.
/// - `config`: scrub configuration.
/// - `device_id`: 16-byte device identifier.
pub fn run_scrub<F, S>(
    device_bitmap_bytes: &[u8],
    mirror_bitmap_bytes: &[u8],
    chunk_ref_extents: &HashSet<usize, S>,
    crc_check: F,
    config: &ScrubConfig,
    device_id: [u8; 16],
) -> ScrubReport
where
    F: Fn(usize) -> bool,
    S: BuildHasher,
{
    let start = Instant::now();
    let mut bitmap_errors: u64 = 0;
    let mut crc_failures: u64 = 0;
    let mut orphan_extents: u64 = 0;
    let mut total_extents_checked: u64 = 0;

    let extent_count = device_bitmap_bytes.len();

    for i in 0..extent_count {
        total_extents_checked += 1;

        // Bitmap consistency: primary vs mirror must match.
        if config.check_bitmap
            && i < mirror_bitmap_bytes.len()
            && device_bitmap_bytes[i] != mirror_bitmap_bytes[i]
        {
            bitmap_errors += 1;
        }

        let allocated = device_bitmap_bytes[i] != 0;

        // Orphan detection: allocated in bitmap but not referenced.
        if config.check_orphans && allocated && !chunk_ref_extents.contains(&i) {
            orphan_extents += 1;
        }

        // CRC sampling: deterministic selection based on sample_rate.
        if allocated && config.sample_rate > 0.0 {
            let hash = simple_hash(i, extent_count);
            if hash < config.sample_rate && !crc_check(i) {
                crc_failures += 1;
            }
        }
    }

    ScrubReport {
        bitmap_errors,
        crc_failures,
        orphan_extents,
        total_extents_checked,
        elapsed: start.elapsed(),
        device_id,
    }
}

/// Cheap deterministic hash mapping an extent index to [0.0, 1.0) for
/// sampling without pulling in a full hasher.
fn simple_hash(index: usize, total: usize) -> f64 {
    if total == 0 {
        return 1.0;
    }
    // Spread with a prime multiplier then normalise.
    #[allow(clippy::unreadable_literal)]
    let h = (index.wrapping_mul(2654435761)) % total;
    #[allow(clippy::cast_precision_loss)]
    let result = h as f64 / total as f64;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_device_produces_clean_report() {
        // All extents allocated and referenced, CRC always valid, bitmaps match.
        let bitmap = vec![1u8; 8];
        let mirror = vec![1u8; 8];
        let refs: HashSet<usize> = (0..8).collect();

        let report = run_scrub(
            &bitmap,
            &mirror,
            &refs,
            |_| true,
            &ScrubConfig::default(),
            [0u8; 16],
        );
        assert!(report.is_clean());
        assert_eq!(report.total_extents_checked, 8);
    }

    #[test]
    fn orphan_extent_detected() {
        // Extent 3 allocated in bitmap but not referenced by any chunk.
        let bitmap = vec![0, 0, 0, 1, 0];
        let mirror = bitmap.clone();
        let refs: HashSet<usize> = HashSet::new(); // no refs at all

        let report = run_scrub(
            &bitmap,
            &mirror,
            &refs,
            |_| true,
            &ScrubConfig::default(),
            [1u8; 16],
        );
        assert_eq!(report.orphan_extents, 1);
        assert_eq!(report.bitmap_errors, 0);
    }

    #[test]
    fn bitmap_error_counted() {
        // Primary says allocated, mirror says free → mismatch.
        let bitmap = vec![1, 0, 0];
        let mirror = vec![0, 0, 0]; // differs at index 0

        let refs: HashSet<usize> = [0].into_iter().collect();

        let report = run_scrub(
            &bitmap,
            &mirror,
            &refs,
            |_| true,
            &ScrubConfig::default(),
            [2u8; 16],
        );
        assert_eq!(report.bitmap_errors, 1);
    }

    #[test]
    fn default_config_values() {
        let cfg = ScrubConfig::default();
        assert!((cfg.sample_rate - 0.1).abs() < f64::EPSILON);
        assert!(cfg.check_bitmap);
        assert!(cfg.check_orphans);
    }
}
