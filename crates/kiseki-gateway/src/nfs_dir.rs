//! NFS directory index — maps filenames to composition IDs within a namespace.
//!
//! Provides the lookup layer needed for NFS LOOKUP, READDIR, and CREATE
//! operations. Each namespace has its own directory tree (flat for MVP,
//! hierarchical later).

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId};

use super::nfs_ops::FileHandle;

/// A directory entry mapping a name to a file handle and composition.
#[derive(Clone, Debug)]
pub struct DirEntry {
    /// Human-readable filename.
    pub name: String,
    /// NFS file handle (32 bytes).
    pub file_handle: FileHandle,
    /// Composition this file maps to.
    pub composition_id: CompositionId,
    /// File size in bytes.
    pub size: u64,
}

/// Directory index — maps (namespace, filename) to directory entries.
///
/// Thread-safe via `Mutex`. Shared between NFS CREATE (writer) and
/// NFS LOOKUP / READDIR (readers).
pub struct DirectoryIndex {
    entries: Mutex<HashMap<NamespaceId, HashMap<String, DirEntry>>>,
}

impl DirectoryIndex {
    /// Create an empty directory index.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Insert a file into the namespace directory.
    pub fn insert(
        &self,
        ns: NamespaceId,
        name: String,
        file_handle: FileHandle,
        composition_id: CompositionId,
        size: u64,
    ) {
        let mut entries = self.entries.lock().unwrap();
        entries.entry(ns).or_default().insert(
            name.clone(),
            DirEntry {
                name,
                file_handle,
                composition_id,
                size,
            },
        );
    }

    /// Look up a file by name in a namespace.
    pub fn lookup(&self, ns: NamespaceId, name: &str) -> Option<DirEntry> {
        let entries = self.entries.lock().unwrap();
        entries.get(&ns).and_then(|dir| dir.get(name).cloned())
    }

    /// List all files in a namespace.
    pub fn list(&self, ns: NamespaceId) -> Vec<DirEntry> {
        let entries = self.entries.lock().unwrap();
        entries
            .get(&ns)
            .map(|dir| dir.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove a file from the namespace directory.
    pub fn remove(&self, ns: NamespaceId, name: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        entries
            .get_mut(&ns)
            .map(|dir| dir.remove(name).is_some())
            .unwrap_or(false)
    }

    /// Number of files in a namespace.
    pub fn count(&self, ns: NamespaceId) -> usize {
        let entries = self.entries.lock().unwrap();
        entries.get(&ns).map_or(0, HashMap::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns1() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(1))
    }

    fn ns2() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(2))
    }

    fn comp(n: u128) -> CompositionId {
        CompositionId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn insert_and_lookup() {
        let idx = DirectoryIndex::new();
        let fh = [0x42u8; 32];
        idx.insert(ns1(), "file.txt".into(), fh, comp(100), 1024);

        let entry = idx.lookup(ns1(), "file.txt").unwrap();
        assert_eq!(entry.name, "file.txt");
        assert_eq!(entry.file_handle, fh);
        assert_eq!(entry.composition_id, comp(100));
        assert_eq!(entry.size, 1024);
    }

    #[test]
    fn lookup_miss() {
        let idx = DirectoryIndex::new();
        assert!(idx.lookup(ns1(), "nope").is_none());
    }

    #[test]
    fn list_entries() {
        let idx = DirectoryIndex::new();
        idx.insert(ns1(), "a.txt".into(), [1; 32], comp(1), 10);
        idx.insert(ns1(), "b.txt".into(), [2; 32], comp(2), 20);
        idx.insert(ns2(), "c.txt".into(), [3; 32], comp(3), 30);

        let ns1_files = idx.list(ns1());
        assert_eq!(ns1_files.len(), 2);

        let ns2_files = idx.list(ns2());
        assert_eq!(ns2_files.len(), 1);

        let empty = idx.list(NamespaceId(uuid::Uuid::from_u128(99)));
        assert!(empty.is_empty());
    }

    #[test]
    fn remove_entry() {
        let idx = DirectoryIndex::new();
        idx.insert(ns1(), "delete_me.txt".into(), [1; 32], comp(1), 10);
        assert_eq!(idx.count(ns1()), 1);

        assert!(idx.remove(ns1(), "delete_me.txt"));
        assert_eq!(idx.count(ns1()), 0);
        assert!(idx.lookup(ns1(), "delete_me.txt").is_none());
    }

    #[test]
    fn remove_nonexistent() {
        let idx = DirectoryIndex::new();
        assert!(!idx.remove(ns1(), "nope"));
    }

    #[test]
    fn namespace_isolation() {
        let idx = DirectoryIndex::new();
        idx.insert(ns1(), "shared_name.txt".into(), [1; 32], comp(1), 10);
        idx.insert(ns2(), "shared_name.txt".into(), [2; 32], comp(2), 20);

        let e1 = idx.lookup(ns1(), "shared_name.txt").unwrap();
        let e2 = idx.lookup(ns2(), "shared_name.txt").unwrap();
        assert_ne!(e1.composition_id, e2.composition_id);
    }
}
