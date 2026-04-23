//! WAL intent journal for crash-safe bitmap updates (I-C8).
//!
//! Bitmap modifications are recorded in the journal before being
//! applied to the on-disk bitmap. On crash recovery, the journal is
//! replayed to reconstruct a consistent bitmap state.

use std::path::{Path, PathBuf};

use crate::extent::Extent;

/// Journal operation type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JournalOp {
    /// Allocate an extent (mark bits as used).
    Alloc,
    /// Free an extent (mark bits as free).
    Free,
}

/// A single journal entry recording a bitmap intent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// Sequence number for ordering.
    pub seq: u64,
    /// Extent offset in bytes.
    pub offset: u64,
    /// Extent length in bytes.
    pub length: u64,
    /// Operation type.
    pub op: JournalOp,
    /// Whether this entry has been applied to the bitmap.
    pub applied: bool,
}

/// WAL journal backed by a file on the metadata partition.
///
/// Entries are appended, then marked as applied after the bitmap
/// update is confirmed. On startup, unapplied entries are replayed.
pub struct Journal {
    path: PathBuf,
    entries: Vec<JournalEntry>,
    next_seq: u64,
}

impl Journal {
    /// Open or create a journal at the given path.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let entries = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            match serde_json::from_str::<Vec<JournalEntry>>(&content) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "journal file corrupt, entries may be lost"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let next_seq = entries.iter().map(|e| e.seq).max().unwrap_or(0) + 1;

        Ok(Self {
            path: path.to_path_buf(),
            entries,
            next_seq,
        })
    }

    /// Record an allocation intent (before modifying bitmap).
    pub fn record_alloc(&mut self, extent: &Extent) -> std::io::Result<u64> {
        self.record(extent, JournalOp::Alloc)
    }

    /// Record a free intent (before modifying bitmap).
    pub fn record_free(&mut self, extent: &Extent) -> std::io::Result<u64> {
        self.record(extent, JournalOp::Free)
    }

    /// Mark an entry as applied (bitmap update confirmed).
    pub fn mark_applied(&mut self, seq: u64) -> std::io::Result<()> {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.seq == seq) {
            entry.applied = true;
        }
        self.persist()
    }

    /// Get unapplied entries for crash recovery replay.
    #[must_use]
    pub fn unapplied(&self) -> Vec<&JournalEntry> {
        self.entries.iter().filter(|e| !e.applied).collect()
    }

    /// Compact the journal: remove all applied entries.
    pub fn compact(&mut self) -> std::io::Result<()> {
        self.entries.retain(|e| !e.applied);
        self.persist()
    }

    /// Number of entries (including applied).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn record(&mut self, extent: &Extent, op: JournalOp) -> std::io::Result<u64> {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push(JournalEntry {
            seq,
            offset: extent.offset,
            length: extent.length,
            op,
            applied: false,
        });
        self.persist()?;
        Ok(seq)
    }

    fn persist(&self) -> std::io::Result<()> {
        let json = serde_json::to_string(&self.entries).map_err(std::io::Error::other)?;
        // Atomic write: write to temp file, then rename (atomic on POSIX).
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_record_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");

        // Record entries.
        {
            let mut j = Journal::open(&path).unwrap();
            let ext = Extent {
                offset: 4096,
                length: 8192,
            };
            j.record_alloc(&ext).unwrap();
            j.record_free(&ext).unwrap();
            assert_eq!(j.len(), 2);
            assert_eq!(j.unapplied().len(), 2);
        }

        // Reopen — entries persisted.
        {
            let j = Journal::open(&path).unwrap();
            assert_eq!(j.len(), 2);
            assert_eq!(j.unapplied().len(), 2);
        }
    }

    #[test]
    fn journal_mark_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");

        let mut j = Journal::open(&path).unwrap();
        let ext = Extent {
            offset: 0,
            length: 4096,
        };
        let seq = j.record_alloc(&ext).unwrap();
        assert_eq!(j.unapplied().len(), 1);

        j.mark_applied(seq).unwrap();
        assert_eq!(j.unapplied().len(), 0);
    }

    #[test]
    fn journal_compact_removes_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");

        let mut j = Journal::open(&path).unwrap();
        let ext = Extent {
            offset: 0,
            length: 4096,
        };
        let s1 = j.record_alloc(&ext).unwrap();
        let _s2 = j.record_free(&ext).unwrap();

        j.mark_applied(s1).unwrap();
        j.compact().unwrap();
        assert_eq!(j.len(), 1); // only unapplied remains
    }

    #[test]
    fn journal_empty_on_fresh_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let j = Journal::open(&path).unwrap();
        assert!(j.is_empty());
    }
}
