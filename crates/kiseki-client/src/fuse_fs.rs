//! FUSE filesystem for Kiseki — translates POSIX ops to `GatewayOps`.
//!
//! Implements a virtual filesystem backed by `GatewayOps`. Each file
//! maps to a composition; the root directory lists compositions in
//! the namespace. Client-side encryption: plaintext never leaves
//! this process (I-K1, I-K2).
//!
//! This module is platform-independent. The actual FUSE mount uses
//! the `fuser` crate (feature-gated) which provides the kernel
//! interface on Linux/macOS.

use std::collections::HashMap;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_gateway::ops::{GatewayOps, ReadRequest, WriteRequest};

/// Inode number type.
pub type Ino = u64;

/// File type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    Regular,
}

/// File attributes returned by `getattr`.
#[derive(Debug, Clone)]
pub struct FileAttr {
    pub ino: Ino,
    pub size: u64,
    pub kind: FileKind,
    pub mode: u32,
    pub nlink: u32,
}

/// Directory entry for `readdir`.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub kind: FileKind,
}

/// Inode table entry.
#[derive(Debug, Clone)]
enum InodeEntry {
    Root,
    File {
        parent: Ino,
        name: String,
        composition_id: CompositionId,
        size: u64,
    },
    Dir {
        parent: Ino,
        name: String,
    },
    Symlink {
        parent: Ino,
        name: String,
        target: String,
    },
}

impl InodeEntry {
    fn parent(&self) -> Option<Ino> {
        match self {
            InodeEntry::Root => None,
            InodeEntry::File { parent, .. }
            | InodeEntry::Dir { parent, .. }
            | InodeEntry::Symlink { parent, .. } => Some(*parent),
        }
    }
}

/// FUSE filesystem backed by `GatewayOps`.
///
/// Maintains an inode table mapping inode numbers to compositions.
/// Thread-safe via interior mutability in the gateway.
pub struct KisekiFuse<G: GatewayOps> {
    gateway: G,
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    inodes: HashMap<Ino, InodeEntry>,
    /// Maps `(parent_ino, child_name)` to child inode.
    children: HashMap<(Ino, String), Ino>,
    next_ino: Ino,
}

impl<G: GatewayOps> KisekiFuse<G> {
    /// Create a new FUSE filesystem.
    pub fn new(gateway: G, tenant_id: OrgId, namespace_id: NamespaceId) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(1, InodeEntry::Root);

        Self {
            gateway,
            tenant_id,
            namespace_id,
            inodes,
            children: HashMap::new(),
            next_ino: 2,
        }
    }

    /// Validate that `ino` is a directory (Root or Dir). Returns error if not.
    fn require_dir(&self, ino: Ino) -> Result<(), i32> {
        match self.inodes.get(&ino) {
            Some(InodeEntry::Root | InodeEntry::Dir { .. }) => Ok(()),
            Some(_) => Err(20), // ENOTDIR
            None => Err(libc_enoent()),
        }
    }

    /// Get file attributes.
    pub fn getattr(&self, ino: Ino) -> Result<FileAttr, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        Ok(match entry {
            InodeEntry::Root | InodeEntry::Dir { .. } => FileAttr {
                ino,
                size: 0,
                kind: FileKind::Directory,
                mode: 0o755,
                nlink: 2,
            },
            InodeEntry::File { size, .. } => FileAttr {
                ino,
                size: *size,
                kind: FileKind::Regular,
                mode: 0o644,
                nlink: 1,
            },
            InodeEntry::Symlink { target, .. } => FileAttr {
                ino,
                size: target.len() as u64,
                kind: FileKind::Regular,
                mode: 0o777,
                nlink: 1,
            },
        })
    }

    /// Look up a name in the given parent directory.
    ///
    /// For backwards compatibility, the single-argument form searches
    /// all entries by name (flat namespace). Use `lookup_in` for proper
    /// nested directory support.
    pub fn lookup(&self, name: &str) -> Result<FileAttr, i32> {
        self.lookup_in(1, name)
    }

    /// Look up a child by name within a specific parent directory.
    pub fn lookup_in(&self, parent: Ino, name: &str) -> Result<FileAttr, i32> {
        self.require_dir(parent)?;
        let ino = self
            .children
            .get(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;
        self.getattr(*ino)
    }

    /// Read from a file.
    pub fn read(&self, ino: Ino, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        let InodeEntry::File { composition_id, .. } = entry else {
            return Err(libc_eisdir());
        };

        self.gateway
            .read(ReadRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                composition_id: *composition_id,
                offset,
                length: u64::from(size),
            })
            .map(|r| r.data)
            .map_err(|_| libc_eio())
    }

    /// Write data to an existing file at a given offset.
    ///
    /// Performs a read-modify-write: reads the full file, splices the
    /// new data at `offset`, then writes the result as a new composition,
    /// updating the inode to point at the new composition.
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        let (old_size, old_composition_id) = match entry {
            InodeEntry::File {
                size,
                composition_id,
                ..
            } => (*size, *composition_id),
            _ => return Err(libc_eisdir()),
        };

        // Read existing data.
        let mut buf = if old_size > 0 {
            self.gateway
                .read(ReadRequest {
                    tenant_id: self.tenant_id,
                    namespace_id: self.namespace_id,
                    composition_id: old_composition_id,
                    offset: 0,
                    length: old_size,
                })
                .map(|r| r.data)
                .map_err(|_| libc_eio())?
        } else {
            Vec::new()
        };

        // Extend buffer if offset + data goes beyond current size.
        #[allow(clippy::cast_possible_truncation)]
        let end = offset as usize + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }

        // Splice new data in.
        #[allow(clippy::cast_possible_truncation)]
        let start = offset as usize;
        buf[start..end].copy_from_slice(data);

        let new_size = buf.len() as u64;

        // Write the full buffer as a new composition.
        let resp = self
            .gateway
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data: buf,
            })
            .map_err(|_| libc_eio())?;

        // Update inode.
        if let Some(InodeEntry::File {
            composition_id,
            size,
            ..
        }) = self.inodes.get_mut(&ino)
        {
            *composition_id = resp.composition_id;
            *size = new_size;
        }

        #[allow(clippy::cast_possible_truncation)]
        let written = data.len() as u32;
        Ok(written)
    }

    /// Write a new file (create + write) under the given parent directory.
    ///
    /// The single-argument `create(name, data)` form creates under root (inode 1).
    /// Use `create_in` for nested directory support.
    pub fn create(&mut self, name: &str, data: Vec<u8>) -> Result<Ino, i32> {
        self.create_in(1, name, data)
    }

    /// Create a file under a specific parent directory.
    pub fn create_in(&mut self, parent: Ino, name: &str, data: Vec<u8>) -> Result<Ino, i32> {
        self.require_dir(parent)?;

        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }

        let size = data.len() as u64;
        let resp = self
            .gateway
            .write(WriteRequest {
                tenant_id: self.tenant_id,
                namespace_id: self.namespace_id,
                data,
            })
            .map_err(|_| libc_eio())?;

        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::File {
                parent,
                name: name.to_owned(),
                composition_id: resp.composition_id,
                size,
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Remove a file from the root directory.
    pub fn unlink(&mut self, name: &str) -> Result<(), i32> {
        self.unlink_in(1, name)
    }

    /// Remove a file from a specific parent directory.
    pub fn unlink_in(&mut self, parent: Ino, name: &str) -> Result<(), i32> {
        self.require_dir(parent)?;
        let ino = self
            .children
            .remove(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;
        self.inodes.remove(&ino);
        Ok(())
    }

    /// List directory entries for the given directory inode.
    ///
    /// The no-argument form lists the root directory (inode 1).
    pub fn readdir(&self) -> Vec<DirEntry> {
        self.readdir_in(1)
    }

    /// List directory entries for a specific directory.
    pub fn readdir_in(&self, dir_ino: Ino) -> Vec<DirEntry> {
        let parent_ino = match self.inodes.get(&dir_ino) {
            Some(InodeEntry::Root) => dir_ino, // root's parent is itself
            Some(InodeEntry::Dir { parent, .. }) => *parent,
            _ => return Vec::new(),
        };

        let mut entries = vec![
            DirEntry {
                ino: dir_ino,
                name: ".".into(),
                kind: FileKind::Directory,
            },
            DirEntry {
                ino: parent_ino,
                name: "..".into(),
                kind: FileKind::Directory,
            },
        ];

        // Iterate children of this directory.
        for (&ino, entry) in &self.inodes {
            if entry.parent() != Some(dir_ino) {
                continue;
            }
            match entry {
                InodeEntry::File { name, .. } | InodeEntry::Symlink { name, .. } => {
                    entries.push(DirEntry {
                        ino,
                        name: name.clone(),
                        kind: FileKind::Regular,
                    });
                }
                InodeEntry::Dir { name, .. } => {
                    entries.push(DirEntry {
                        ino,
                        name: name.clone(),
                        kind: FileKind::Directory,
                    });
                }
                InodeEntry::Root => {}
            }
        }

        entries
    }

    /// Create a directory under root.
    pub fn mkdir(&mut self, name: &str) -> Result<Ino, i32> {
        self.mkdir_in(1, name)
    }

    /// Create a directory under a specific parent.
    pub fn mkdir_in(&mut self, parent: Ino, name: &str) -> Result<Ino, i32> {
        self.require_dir(parent)?;

        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }

        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::Dir {
                parent,
                name: name.to_owned(),
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Remove a directory from root.
    pub fn rmdir(&mut self, name: &str) -> Result<(), i32> {
        self.rmdir_in(1, name)
    }

    /// Remove a directory from a specific parent.
    pub fn rmdir_in(&mut self, parent: Ino, name: &str) -> Result<(), i32> {
        self.require_dir(parent)?;
        let ino = *self
            .children
            .get(&(parent, name.to_owned()))
            .ok_or(libc_enoent())?;
        if !matches!(self.inodes.get(&ino), Some(InodeEntry::Dir { .. })) {
            return Err(20); // ENOTDIR
        }
        // Check directory is empty.
        let has_children = self.children.keys().any(|(p, _)| *p == ino);
        if has_children {
            return Err(39); // ENOTEMPTY (macOS) / 66 on some systems
        }
        self.children.remove(&(parent, name.to_owned()));
        self.inodes.remove(&ino);
        Ok(())
    }

    /// Rename a file or directory within the root directory.
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> Result<(), i32> {
        self.rename_in(1, old_name, 1, new_name)
    }

    /// Rename a file or directory, possibly moving between parents.
    pub fn rename_in(
        &mut self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
    ) -> Result<(), i32> {
        self.require_dir(old_parent)?;
        self.require_dir(new_parent)?;

        let ino = self
            .children
            .remove(&(old_parent, old_name.to_owned()))
            .ok_or(libc_enoent())?;

        // Update the name (and parent) in the inode entry.
        if let Some(entry) = self.inodes.get_mut(&ino) {
            match entry {
                InodeEntry::File { name, parent, .. }
                | InodeEntry::Dir { name, parent }
                | InodeEntry::Symlink { name, parent, .. } => {
                    new_name.clone_into(name);
                    *parent = new_parent;
                }
                InodeEntry::Root => return Err(libc_eio()),
            }
        }
        // Remove any existing entry at new_name (overwrite semantics).
        if let Some(old_ino) = self.children.remove(&(new_parent, new_name.to_owned())) {
            self.inodes.remove(&old_ino);
        }
        self.children.insert((new_parent, new_name.to_owned()), ino);
        Ok(())
    }

    /// Set file attributes (mode only for now).
    pub fn setattr(&mut self, ino: Ino, _mode: Option<u32>) -> Result<FileAttr, i32> {
        // Attributes are computed, not stored (in-memory FS).
        // Return current attributes unchanged.
        self.getattr(ino)
    }

    /// Create a symbolic link under root.
    pub fn symlink(&mut self, name: &str, target: &str) -> Result<Ino, i32> {
        self.symlink_in(1, name, target)
    }

    /// Create a symbolic link under a specific parent.
    pub fn symlink_in(&mut self, parent: Ino, name: &str, target: &str) -> Result<Ino, i32> {
        self.require_dir(parent)?;
        if self.children.contains_key(&(parent, name.to_owned())) {
            return Err(17); // EEXIST
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::Symlink {
                parent,
                name: name.to_owned(),
                target: target.to_owned(),
            },
        );
        self.children.insert((parent, name.to_owned()), ino);
        Ok(ino)
    }

    /// Read a symbolic link target.
    pub fn readlink(&self, ino: Ino) -> Result<String, i32> {
        let entry = self.inodes.get(&ino).ok_or(libc_enoent())?;
        match entry {
            InodeEntry::Symlink { target, .. } => Ok(target.clone()),
            _ => Err(22), // EINVAL
        }
    }
}

fn libc_enoent() -> i32 {
    2 // ENOENT
}
fn libc_eio() -> i32 {
    5 // EIO
}
fn libc_eisdir() -> i32 {
    21 // EISDIR
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::ids::ShardId;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_gateway::mem_gateway::InMemoryGateway;

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    fn test_namespace() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_u128(200))
    }

    fn setup_fuse() -> KisekiFuse<InMemoryGateway> {
        let mut compositions = CompositionStore::new();
        compositions.add_namespace(Namespace {
            id: test_namespace(),
            tenant_id: test_tenant(),
            shard_id: ShardId(uuid::Uuid::from_u128(1)),
            read_only: false,
        });
        let chunks = ChunkStore::new();
        let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let gateway = InMemoryGateway::new(compositions, Box::new(chunks), master_key);

        KisekiFuse::new(gateway, test_tenant(), test_namespace())
    }

    #[test]
    fn root_getattr() {
        let fs = setup_fuse();
        let attr = fs.getattr(1).unwrap();
        assert_eq!(attr.kind, FileKind::Directory);
        assert_eq!(attr.ino, 1);
    }

    #[test]
    fn create_and_read_file() {
        let mut fs = setup_fuse();
        let data = b"hello fuse world";

        let ino = fs.create("test.txt", data.to_vec()).unwrap();
        assert!(ino >= 2);

        // Read back.
        let read_data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(read_data, data);
    }

    #[test]
    fn lookup_file() {
        let mut fs = setup_fuse();
        fs.create("myfile.txt", b"content".to_vec()).unwrap();

        let attr = fs.lookup("myfile.txt").unwrap();
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 7);
    }

    #[test]
    fn lookup_nonexistent_returns_enoent() {
        let fs = setup_fuse();
        assert_eq!(fs.lookup("nope").unwrap_err(), 2); // ENOENT
    }

    #[test]
    fn unlink_file() {
        let mut fs = setup_fuse();
        fs.create("deleteme.txt", b"data".to_vec()).unwrap();
        assert!(fs.lookup("deleteme.txt").is_ok());

        fs.unlink("deleteme.txt").unwrap();
        assert_eq!(fs.lookup("deleteme.txt").unwrap_err(), 2);
    }

    #[test]
    fn readdir_lists_files() {
        let mut fs = setup_fuse();
        fs.create("a.txt", b"aaa".to_vec()).unwrap();
        fs.create("b.txt", b"bbb".to_vec()).unwrap();

        let entries = fs.readdir();
        assert!(entries.len() >= 4); // . + .. + a.txt + b.txt
        assert!(entries.iter().any(|e| e.name == "a.txt"));
        assert!(entries.iter().any(|e| e.name == "b.txt"));
    }

    #[test]
    fn read_with_offset() {
        let mut fs = setup_fuse();
        let data = b"abcdefghijklmnop";
        let ino = fs.create("offset.txt", data.to_vec()).unwrap();

        let chunk = fs.read(ino, 4, 4).unwrap();
        assert_eq!(chunk, b"efgh");
    }

    #[test]
    fn nested_directory_create_and_lookup() {
        let mut fs = setup_fuse();

        // Create a subdirectory under root.
        let subdir_ino = fs.mkdir("subdir").unwrap();
        assert!(subdir_ino >= 2);

        // Verify subdirectory appears in root listing.
        let root_entries = fs.readdir();
        assert!(root_entries
            .iter()
            .any(|e| e.name == "subdir" && e.kind == FileKind::Directory));

        // Create a file inside the subdirectory.
        let file_ino = fs
            .create_in(subdir_ino, "nested.txt", b"nested data".to_vec())
            .unwrap();
        assert!(file_ino > subdir_ino);

        // Look up the file by name within the subdirectory.
        let attr = fs.lookup_in(subdir_ino, "nested.txt").unwrap();
        assert_eq!(attr.kind, FileKind::Regular);
        assert_eq!(attr.size, 11);
        assert_eq!(attr.ino, file_ino);

        // The file must NOT appear in the root directory listing.
        let root_entries = fs.readdir();
        assert!(!root_entries.iter().any(|e| e.name == "nested.txt"));

        // The file MUST appear in the subdirectory listing.
        let sub_entries = fs.readdir_in(subdir_ino);
        assert!(sub_entries.iter().any(|e| e.name == "nested.txt"));
        // Subdirectory listing includes . and ..
        assert!(sub_entries.iter().any(|e| e.name == "."));
        assert!(sub_entries.iter().any(|e| e.name == ".."));

        // Look up in root must fail for the nested file.
        assert!(fs.lookup_in(1, "nested.txt").is_err());

        // Read the file data to confirm it works.
        let data = fs.read(file_ino, 0, 1024).unwrap();
        assert_eq!(data, b"nested data");
    }

    #[test]
    fn write_at_offset() {
        let mut fs = setup_fuse();

        // Create a file with initial content.
        let ino = fs.create("wfile.txt", b"Hello, World!".to_vec()).unwrap();

        // Verify initial content.
        let data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(data, b"Hello, World!");

        // Write at offset 5 — replace ", World!" with " Rust!"
        let written = fs.write(ino, 5, b" Rust!").unwrap();
        assert_eq!(written, 6);

        // Read back the full file. Length should be max(old_len, offset+new_len).
        // "Hello, World!" is 13 bytes, offset 5 + 6 = 11, so file is still 13.
        let data = fs.read(ino, 0, 1024).unwrap();
        assert_eq!(data, b"Hello Rust!d!");

        // Write beyond end of file — extends the file.
        let written = fs.write(ino, 15, b"XY").unwrap();
        assert_eq!(written, 2);

        let data = fs.read(ino, 0, 1024).unwrap();
        // Bytes 13..15 should be zero-filled.
        assert_eq!(data.len(), 17);
        assert_eq!(&data[13..15], &[0, 0]);
        assert_eq!(&data[15..17], b"XY");
    }
}
