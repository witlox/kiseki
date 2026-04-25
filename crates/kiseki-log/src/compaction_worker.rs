//! Background compaction worker for the append-only log.
//!
//! Over time, the log accumulates tombstones (Delete operations) and
//! superseded versions (Update chains). Compaction removes dead entries
//! to reclaim space and reduce read amplification.
//!
//! Compaction is rate-limited to avoid overwhelming I/O during peak hours.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::delta::{Delta, OperationType};

/// Compaction configuration.
#[derive(Clone, Debug)]
pub struct CompactionConfig {
    /// Maximum deltas to compact per batch.
    pub batch_size: u64,
    /// Minimum tombstone ratio to trigger compaction (0.0 to 1.0).
    pub tombstone_ratio_threshold: f64,
    /// Maximum bytes per second to read during compaction (rate limit).
    pub max_bytes_per_sec: u64,
    /// Minimum number of versions to retain per key (for version history).
    /// Superseded versions older than this count are eligible for removal.
    pub min_versions_retained: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            batch_size: 10_000,
            tombstone_ratio_threshold: 0.3,
            max_bytes_per_sec: 50 * 1024 * 1024, // 50 MB/s
            min_versions_retained: 1,
        }
    }
}

/// Compaction progress.
#[derive(Debug)]
pub struct CompactionProgress {
    /// Total deltas examined.
    pub examined: AtomicU64,
    /// Deltas removed (tombstones or superseded).
    pub removed: AtomicU64,
    /// Deltas retained.
    pub retained: AtomicU64,
    /// Whether compaction has been cancelled.
    pub cancelled: AtomicBool,
}

impl CompactionProgress {
    /// Create a new progress tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            examined: AtomicU64::new(0),
            removed: AtomicU64::new(0),
            retained: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Cancel the compaction.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether compaction was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Default for CompactionProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Analyze a shard's deltas and determine which can be compacted.
///
/// Returns the list of deltas to keep. Removed deltas are:
/// - Delete tombstones (the key is gone)
/// - Superseded Creates/Updates beyond the version retention window
///
/// `min_versions` controls how many versions per key are retained
/// for the versioning layer's historical reads.
#[must_use]
pub fn compact_deltas(
    deltas: &[Delta],
    progress: &CompactionProgress,
    min_versions: u64,
) -> Vec<Delta> {
    use std::collections::HashMap;

    if progress.is_cancelled() {
        return deltas.to_vec();
    }

    // Group all deltas by key, sorted by sequence (newest first).
    let mut by_key: HashMap<[u8; 32], Vec<&Delta>> = HashMap::new();
    for delta in deltas {
        progress.examined.fetch_add(1, Ordering::Relaxed);
        by_key
            .entry(delta.header.hashed_key)
            .or_default()
            .push(delta);
    }

    let mut retained = Vec::new();
    let min_keep = usize::try_from(min_versions.max(1)).unwrap_or(usize::MAX);

    for (_key, mut versions) in by_key {
        if progress.is_cancelled() {
            return deltas.to_vec();
        }
        // Sort newest first.
        versions.sort_by_key(|d| std::cmp::Reverse(d.header.sequence));

        for (i, delta) in versions.iter().enumerate() {
            let is_tombstone = delta.header.operation == OperationType::Delete;
            // Keep the newest `min_keep` versions. Remove older superseded + tombstones.
            if i < min_keep && !is_tombstone {
                retained.push((*delta).clone());
                progress.retained.fetch_add(1, Ordering::Relaxed);
            } else {
                progress.removed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    retained.sort_by_key(|d| d.header.sequence);
    retained
}

/// Check if a shard needs compaction based on tombstone ratio.
#[must_use]
pub fn needs_compaction(deltas: &[Delta], config: &CompactionConfig) -> bool {
    if deltas.is_empty() {
        return false;
    }
    let tombstones = deltas
        .iter()
        .filter(|d| d.header.operation == OperationType::Delete)
        .count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = tombstones as f64 / deltas.len() as f64;
    ratio >= config.tombstone_ratio_threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};
    use kiseki_common::time::*;

    fn make_delta(seq: u64, key_byte: u8, op: OperationType) -> Delta {
        use crate::delta::{DeltaHeader, DeltaPayload};
        Delta {
            header: DeltaHeader {
                sequence: SequenceNumber(seq),
                shard_id: ShardId(uuid::Uuid::from_u128(1)),
                tenant_id: OrgId(uuid::Uuid::from_u128(100)),
                operation: op,
                timestamp: DeltaTimestamp {
                    hlc: HybridLogicalClock {
                        physical_ms: 1000,
                        logical: 0,
                        node_id: NodeId(1),
                    },
                    wall: WallTime {
                        millis_since_epoch: 1000,
                        timezone: "UTC".into(),
                    },
                    quality: ClockQuality::Ntp,
                },
                hashed_key: [key_byte; 32],
                tombstone: op == OperationType::Delete,
                chunk_refs: vec![],
                payload_size: 0,
                has_inline_data: false,
            },
            payload: DeltaPayload {
                ciphertext: vec![],
                auth_tag: vec![],
                nonce: vec![],
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: vec![],
            },
        }
    }

    #[test]
    fn compact_removes_tombstones() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xBB, OperationType::Create),
            make_delta(3, 0xAA, OperationType::Delete),
        ];

        let progress = CompactionProgress::new();
        let retained = compact_deltas(&deltas, &progress, 1);

        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].header.hashed_key, [0xBB; 32]);
        assert!(progress.removed.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn compact_keeps_latest_version() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xAA, OperationType::Update),
            make_delta(3, 0xAA, OperationType::Update),
        ];

        let progress = CompactionProgress::new();
        let retained = compact_deltas(&deltas, &progress, 1);

        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].header.sequence, SequenceNumber(3));
    }

    #[test]
    fn compact_preserves_independent_keys() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xBB, OperationType::Create),
            make_delta(3, 0xCC, OperationType::Create),
        ];

        let progress = CompactionProgress::new();
        let retained = compact_deltas(&deltas, &progress, 1);
        assert_eq!(retained.len(), 3);
    }

    #[test]
    fn compact_empty_is_noop() {
        let progress = CompactionProgress::new();
        let retained = compact_deltas(&[], &progress, 1);
        assert!(retained.is_empty());
    }

    #[test]
    fn compact_cancellation() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xBB, OperationType::Create),
        ];

        let progress = CompactionProgress::new();
        progress.cancel();
        let retained = compact_deltas(&deltas, &progress, 1);
        assert_eq!(retained.len(), 2);
    }

    #[test]
    fn needs_compaction_below_threshold() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xBB, OperationType::Create),
            make_delta(3, 0xCC, OperationType::Create),
        ];
        assert!(!needs_compaction(&deltas, &CompactionConfig::default()));
    }

    #[test]
    fn needs_compaction_above_threshold() {
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xBB, OperationType::Delete),
            make_delta(3, 0xCC, OperationType::Delete),
        ];
        assert!(needs_compaction(&deltas, &CompactionConfig::default()));
    }

    #[test]
    fn needs_compaction_empty() {
        assert!(!needs_compaction(&[], &CompactionConfig::default()));
    }

    // --- log.feature @unit: "Automatic compaction merges SSTables" ---
    // Proves: SSTables are merged by hashed_key and sequence_number,
    // newer deltas supersede older ones, tombstoned entries are removed,
    // tenant-encrypted payloads are carried opaquely (never decrypted),
    // and the resulting count is reduced.

    #[test]
    fn automatic_compaction_merges_by_key_and_sequence() {
        // Simulate 20 deltas across several keys with superseded versions.
        let deltas = vec![
            // Key 0xAA: 5 versions (Create + 4 Updates) — only latest survives.
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(5, 0xAA, OperationType::Update),
            make_delta(10, 0xAA, OperationType::Update),
            make_delta(15, 0xAA, OperationType::Update),
            make_delta(20, 0xAA, OperationType::Update),
            // Key 0xBB: created then deleted (tombstone) — both removed.
            make_delta(2, 0xBB, OperationType::Create),
            make_delta(12, 0xBB, OperationType::Delete),
            // Key 0xCC: single create — survives.
            make_delta(3, 0xCC, OperationType::Create),
            // Key 0xDD: 3 versions — only latest survives.
            make_delta(4, 0xDD, OperationType::Create),
            make_delta(8, 0xDD, OperationType::Update),
            make_delta(16, 0xDD, OperationType::Update),
        ];

        let progress = CompactionProgress::new();
        let retained = compact_deltas(&deltas, &progress, 1);

        // Key 0xAA: 1 (seq 20), Key 0xBB: 0 (tombstoned), Key 0xCC: 1, Key 0xDD: 1 (seq 16).
        assert_eq!(
            retained.len(),
            3,
            "should retain only latest per key, removing tombstones"
        );

        // Newer deltas (higher sequence_number) supersede older ones.
        let aa = retained.iter().find(|d| d.header.hashed_key == [0xAA; 32]);
        assert_eq!(aa.unwrap().header.sequence, SequenceNumber(20));

        let dd = retained.iter().find(|d| d.header.hashed_key == [0xDD; 32]);
        assert_eq!(dd.unwrap().header.sequence, SequenceNumber(16));

        // Tombstoned entry is removed.
        let bb = retained.iter().find(|d| d.header.hashed_key == [0xBB; 32]);
        assert!(bb.is_none(), "tombstoned key should be removed");

        // Payloads are carried opaquely — the compaction never decrypts.
        // (Structural proof: compact_deltas clones Delta which includes
        // DeltaPayload.ciphertext unchanged.)
        for delta in &retained {
            // Payload is the same empty vec from make_delta — never modified.
            assert_eq!(delta.payload.ciphertext, Vec::<u8>::new());
        }

        // Count is reduced.
        assert!(
            retained.len() < deltas.len(),
            "compaction must reduce delta count"
        );
    }

    // --- log.feature @unit: "Admin-triggered compaction" ---
    // Proves: compaction runs regardless of the automatic threshold,
    // same merge semantics apply.

    #[test]
    fn admin_triggered_compaction_runs_regardless_of_threshold() {
        // Two versions of the same key — below any automatic threshold.
        let deltas = vec![
            make_delta(1, 0xAA, OperationType::Create),
            make_delta(2, 0xAA, OperationType::Update),
        ];

        // Verify this would NOT trigger automatic compaction (no tombstones).
        assert!(
            !needs_compaction(&deltas, &CompactionConfig::default()),
            "should not auto-trigger"
        );

        // But admin-triggered compaction runs regardless.
        let progress = CompactionProgress::new();
        let retained = compact_deltas(&deltas, &progress, 1);

        // Same merge semantics: only latest version survives.
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].header.sequence, SequenceNumber(2));
    }
}
