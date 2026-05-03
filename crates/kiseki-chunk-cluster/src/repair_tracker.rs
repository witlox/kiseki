//! Recent-repair history for the storage-admin `ListRepairs` RPC.
//!
//! The scrub scheduler doesn't otherwise persist what it repaired —
//! `ScrubReport` is a per-pass count, not a per-fragment audit
//! trail. Operators need the per-fragment view ("what did the scrub
//! touch in the last hour?") for capacity planning + post-incident
//! review.
//!
//! Design: a fixed-capacity ring buffer of `RepairRecord`s, written
//! to by the scrub scheduler (one entry per fragment repaired or
//! attempted) and read by the storage-admin RPC. Sharing is via
//! `Arc<RepairTracker>`; the buffer's mutex is held only across
//! cheap pushes / clones, never across IO.
//!
//! Bound: 4096 records ≈ ~1 MB of memory. When full, oldest records
//! are dropped — the use case is "recent activity", not historical.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::SystemTime;

use kiseki_common::ids::ChunkId;

/// Ring-buffer cap. Sized to comfortably hold a day's worth of
/// fragment repair attempts on a busy cluster (a few per minute).
const DEFAULT_CAPACITY: usize = 4096;

/// What triggered a repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairTrigger {
    /// Triggered by the periodic scrub scheduler.
    Scrub,
    /// Triggered by an operator via `StorageAdminService.RepairChunk`.
    Manual,
    /// Triggered by under-replication detection during a write fan-out.
    UnderReplication,
}

impl RepairTrigger {
    /// Wire-format string used by the proto `RepairRecord.trigger` field.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Scrub => "scrub",
            Self::Manual => "manual",
            Self::UnderReplication => "under_replication",
        }
    }
}

/// Outcome of a repair attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepairState {
    /// Still running.
    InProgress,
    /// Finished with no error.
    Succeeded,
    /// Finished with an error.
    Failed,
}

impl RepairState {
    /// Wire-format string used by the proto `RepairRecord.state` field.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// One fragment-repair entry in the ring.
#[derive(Clone, Debug)]
pub struct RepairRecord {
    /// Stable identifier; UUID v4 minted on creation.
    pub repair_id: String,
    /// Chunk being repaired.
    pub chunk_id: ChunkId,
    /// What triggered the repair.
    pub trigger: RepairTrigger,
    /// Lifecycle state — `InProgress` at start; `Succeeded` / `Failed` at finish.
    pub state: RepairState,
    /// Wall-clock at start (Unix-millis).
    pub started_at_ms: u64,
    /// Wall-clock at finish (Unix-millis); `None` while
    /// `state == InProgress`.
    pub finished_at_ms: Option<u64>,
    /// Free-form detail — e.g. peer name, error message. Up to ~256 chars.
    pub detail: String,
}

/// Shared repair-history buffer.
#[derive(Debug)]
pub struct RepairTracker {
    inner: Mutex<VecDeque<RepairRecord>>,
    capacity: usize,
}

impl Default for RepairTracker {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl RepairTracker {
    /// Construct an empty tracker with the default capacity (4096).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a non-default ring capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    /// Push a new record. Drops the oldest entry when the ring is full.
    /// Returns the record's id so callers can update state later via
    /// [`Self::update_state`].
    pub fn record(&self, mut rec: RepairRecord) -> String {
        if rec.repair_id.is_empty() {
            rec.repair_id = uuid::Uuid::new_v4().to_string();
        }
        let id = rec.repair_id.clone();
        if let Ok(mut q) = self.inner.lock() {
            if q.len() >= self.capacity {
                q.pop_front();
            }
            q.push_back(rec);
        }
        id
    }

    /// Convenience constructor: insert a record in the `InProgress`
    /// state and return its id.
    pub fn start(
        &self,
        trigger: RepairTrigger,
        chunk_id: ChunkId,
        detail: impl Into<String>,
    ) -> String {
        self.record(RepairRecord {
            repair_id: String::new(),
            chunk_id,
            trigger,
            state: RepairState::InProgress,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            detail: detail.into(),
        })
    }

    /// Move an existing record to a terminal state. Silently no-ops
    /// if `id` was evicted from the ring.
    pub fn update_state(&self, id: &str, state: RepairState, detail: Option<String>) {
        if let Ok(mut q) = self.inner.lock() {
            if let Some(rec) = q.iter_mut().find(|r| r.repair_id == id) {
                rec.state = state;
                rec.finished_at_ms = Some(now_ms());
                if let Some(d) = detail {
                    rec.detail = d;
                }
            }
        }
    }

    /// Return up to `limit` most-recent records. Newest first.
    /// `limit == 0` is treated as "all".
    pub fn recent(&self, limit: usize) -> Vec<RepairRecord> {
        let Ok(q) = self.inner.lock() else {
            return Vec::new();
        };
        let max = if limit == 0 || limit > q.len() {
            q.len()
        } else {
            limit
        };
        q.iter().rev().take(max).cloned().collect()
    }

    /// Number of records currently in the ring. Test-only.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.len())
    }

    /// Whether the ring is empty. Test-only counterpart to `len()`.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(b: u8) -> ChunkId {
        ChunkId([b; 32])
    }

    #[test]
    fn record_returns_assigned_id() {
        let t = RepairTracker::new();
        let id = t.start(RepairTrigger::Manual, cid(1), "manual repair");
        assert!(!id.is_empty());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let t = RepairTracker::with_capacity(3);
        for i in 0..5 {
            t.start(RepairTrigger::Scrub, cid(i), format!("entry {i}"));
        }
        assert_eq!(t.len(), 3, "ring must evict to stay at capacity");
        let recent = t.recent(0);
        // Newest first → indices 4, 3, 2 (the survivors).
        assert_eq!(recent[0].chunk_id, cid(4));
        assert_eq!(recent[1].chunk_id, cid(3));
        assert_eq!(recent[2].chunk_id, cid(2));
    }

    #[test]
    fn update_state_moves_record_to_terminal() {
        let t = RepairTracker::new();
        let id = t.start(RepairTrigger::UnderReplication, cid(7), "");
        t.update_state(&id, RepairState::Succeeded, Some("ok".into()));
        let r = t.recent(1).into_iter().next().expect("recent");
        assert_eq!(r.state, RepairState::Succeeded);
        assert_eq!(r.detail, "ok");
        assert!(r.finished_at_ms.is_some());
    }

    #[test]
    fn update_state_on_evicted_id_is_silent() {
        let t = RepairTracker::with_capacity(2);
        let id = t.start(RepairTrigger::Manual, cid(1), "");
        t.start(RepairTrigger::Manual, cid(2), "");
        t.start(RepairTrigger::Manual, cid(3), ""); // evicts id
        t.update_state(&id, RepairState::Succeeded, None); // no panic
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn recent_with_limit_caps_response() {
        let t = RepairTracker::new();
        for i in 0..10 {
            t.start(RepairTrigger::Scrub, cid(i), "");
        }
        assert_eq!(t.recent(3).len(), 3);
        assert_eq!(t.recent(0).len(), 10);
        assert_eq!(t.recent(100).len(), 10);
    }

    #[test]
    fn wire_strings_match_proto_contract() {
        // The `as_wire()` strings appear verbatim in proto
        // RepairRecord.trigger / .state. Pin them so a future rename
        // can't silently break the wire format.
        assert_eq!(RepairTrigger::Scrub.as_wire(), "scrub");
        assert_eq!(RepairTrigger::Manual.as_wire(), "manual");
        assert_eq!(
            RepairTrigger::UnderReplication.as_wire(),
            "under_replication"
        );
        assert_eq!(RepairState::InProgress.as_wire(), "in_progress");
        assert_eq!(RepairState::Succeeded.as_wire(), "succeeded");
        assert_eq!(RepairState::Failed.as_wire(), "failed");
    }
}
