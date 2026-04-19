//! Multipart upload FSM (I-L5).
//!
//! A multipart upload goes through: Started → parts uploaded →
//! Finalized (visible to readers) or Aborted.

use kiseki_common::ids::ChunkId;

/// Multipart upload state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MultipartState {
    /// Upload started, accepting parts.
    InProgress,
    /// All parts confirmed durable, composition visible to readers.
    Finalized,
    /// Upload aborted, parts eligible for GC.
    Aborted,
}

/// A multipart upload tracking structure.
#[derive(Clone, Debug)]
pub struct MultipartUpload {
    /// Unique upload identifier.
    pub upload_id: String,
    /// Current state.
    pub state: MultipartState,
    /// Parts uploaded so far (ordered by part number).
    pub parts: Vec<MultipartPart>,
}

/// A single part of a multipart upload.
#[derive(Clone, Debug)]
pub struct MultipartPart {
    /// Part number (1-based).
    pub part_number: u32,
    /// Chunk ID for this part's data.
    pub chunk_id: ChunkId,
    /// Size in bytes.
    pub size: u64,
}

impl MultipartUpload {
    /// Start a new multipart upload.
    #[must_use]
    pub fn new(upload_id: String) -> Self {
        Self {
            upload_id,
            state: MultipartState::InProgress,
            parts: Vec::new(),
        }
    }

    /// Add a part. Only allowed in `InProgress` state.
    pub fn add_part(&mut self, part: MultipartPart) -> bool {
        if self.state != MultipartState::InProgress {
            return false;
        }
        self.parts.push(part);
        true
    }

    /// Finalize the upload — makes the composition visible (I-L5).
    pub fn finalize(&mut self) -> bool {
        if self.state != MultipartState::InProgress {
            return false;
        }
        self.state = MultipartState::Finalized;
        true
    }

    /// Abort the upload.
    pub fn abort(&mut self) -> bool {
        if self.state != MultipartState::InProgress {
            return false;
        }
        self.state = MultipartState::Aborted;
        true
    }

    /// Total size across all parts.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.parts.iter().map(|p| p.size).sum()
    }
}
