//! pNFS (parallel NFS) layout support.
//!
//! Implements file-level layout delegation per RFC 5661/5663.
//! Clients can perform direct I/O to storage devices after
//! obtaining a layout from the metadata server.

/// pNFS layout type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutType {
    /// File-based layout (RFC 5661 §13).
    File,
    /// Block-based layout (RFC 5663).
    Block,
}

/// A single device mapping within a layout.
#[derive(Clone, Debug)]
pub struct LayoutSegment {
    /// Offset in the file.
    pub offset: u64,
    /// Length of this segment.
    pub length: u64,
    /// Storage node address holding this segment's data.
    pub device_addr: String,
    /// Device-specific identifier (chunk ID or extent reference).
    pub device_id: Vec<u8>,
    /// Whether this segment is for read, write, or both.
    pub iomode: IoMode,
}

/// I/O mode for a layout segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoMode {
    /// Read-only access.
    Read,
    /// Read-write access.
    ReadWrite,
}

/// pNFS layout for a file.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Layout type.
    pub layout_type: LayoutType,
    /// File identifier.
    pub file_id: u64,
    /// Segments making up the layout.
    pub segments: Vec<LayoutSegment>,
    /// Layout stateid (opaque, for return/recall).
    pub stateid: [u8; 16],
}

/// pNFS layout manager.
pub struct LayoutManager {
    /// Active layouts keyed by `file_id`.
    layouts: std::collections::HashMap<u64, Layout>,
    /// Storage node addresses for device ID resolution.
    storage_nodes: Vec<String>,
}

impl LayoutManager {
    /// Create a new layout manager with the given storage node addresses.
    #[must_use]
    pub fn new(storage_nodes: Vec<String>) -> Self {
        Self {
            layouts: std::collections::HashMap::new(),
            storage_nodes,
        }
    }

    /// LAYOUTGET: compute a layout for a file.
    ///
    /// Returns segments distributed across available storage nodes
    /// using round-robin striping.
    pub fn layout_get(&mut self, file_id: u64, offset: u64, length: u64, iomode: IoMode) -> Layout {
        if let Some(existing) = self.layouts.get(&file_id) {
            return existing.clone();
        }

        let stripe_size: u64 = 1024 * 1024; // 1 MiB stripes
        let num_nodes = self.storage_nodes.len().max(1);
        let mut segments = Vec::new();
        let mut pos = offset;
        let end = offset + length;

        while pos < end {
            let seg_len = stripe_size.min(end - pos);
            #[allow(clippy::cast_possible_truncation)]
            let node_idx = ((pos / stripe_size) as usize) % num_nodes;
            segments.push(LayoutSegment {
                offset: pos,
                length: seg_len,
                device_addr: self
                    .storage_nodes
                    .get(node_idx)
                    .cloned()
                    .unwrap_or_else(|| "localhost:9100".into()),
                device_id: file_id.to_le_bytes().to_vec(),
                iomode,
            });
            pos += seg_len;
        }

        // Generate a stateid.
        let mut stateid = [0u8; 16];
        stateid[..8].copy_from_slice(&file_id.to_le_bytes());
        stateid[8..16].copy_from_slice(&offset.to_le_bytes());

        let layout = Layout {
            layout_type: LayoutType::File,
            file_id,
            segments,
            stateid,
        };
        self.layouts.insert(file_id, layout.clone());
        layout
    }

    /// LAYOUTRETURN: release a layout.
    pub fn layout_return(&mut self, file_id: u64) -> bool {
        self.layouts.remove(&file_id).is_some()
    }

    /// Get number of active layouts.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.layouts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_nodes() -> Vec<String> {
        vec![
            "node1:9100".into(),
            "node2:9100".into(),
            "node3:9100".into(),
        ]
    }

    #[test]
    fn layout_get_covers_full_range() {
        let mut mgr = LayoutManager::new(test_nodes());
        let layout = mgr.layout_get(1, 0, 4 * 1024 * 1024, IoMode::Read);

        let total: u64 = layout.segments.iter().map(|s| s.length).sum();
        assert_eq!(total, 4 * 1024 * 1024);
        assert_eq!(layout.segments.first().unwrap().offset, 0);

        // Verify contiguous coverage.
        for w in layout.segments.windows(2) {
            assert_eq!(w[0].offset + w[0].length, w[1].offset);
        }
    }

    #[test]
    fn segments_distributed_across_nodes() {
        let mut mgr = LayoutManager::new(test_nodes());
        let layout = mgr.layout_get(1, 0, 3 * 1024 * 1024, IoMode::ReadWrite);

        let addrs: Vec<&str> = layout
            .segments
            .iter()
            .map(|s| s.device_addr.as_str())
            .collect();
        assert_eq!(addrs, vec!["node1:9100", "node2:9100", "node3:9100"]);
    }

    #[test]
    fn layout_return_removes_layout() {
        let mut mgr = LayoutManager::new(test_nodes());
        mgr.layout_get(42, 0, 1024 * 1024, IoMode::Read);
        assert_eq!(mgr.active_count(), 1);

        assert!(mgr.layout_return(42));
        assert_eq!(mgr.active_count(), 0);

        // Returning again yields false.
        assert!(!mgr.layout_return(42));
    }

    #[test]
    fn repeat_layout_get_returns_cached() {
        let mut mgr = LayoutManager::new(test_nodes());
        let l1 = mgr.layout_get(7, 0, 2 * 1024 * 1024, IoMode::Read);
        let l2 = mgr.layout_get(7, 0, 2 * 1024 * 1024, IoMode::Read);

        assert_eq!(l1.stateid, l2.stateid);
        assert_eq!(l1.segments.len(), l2.segments.len());
        assert_eq!(mgr.active_count(), 1);
    }
}
