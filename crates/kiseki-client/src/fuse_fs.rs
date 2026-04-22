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
        name: String,
        composition_id: CompositionId,
        size: u64,
    },
    Dir {
        name: String,
    },
    Symlink {
        name: String,
        target: String,
    },
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
    name_to_ino: HashMap<String, Ino>,
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
            name_to_ino: HashMap::new(),
            next_ino: 2,
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

    /// Look up a name in the root directory.
    pub fn lookup(&self, name: &str) -> Result<FileAttr, i32> {
        let ino = self.name_to_ino.get(name).ok_or(libc_enoent())?;
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

    /// Write a new file (create + write).
    pub fn create(&mut self, name: &str, data: Vec<u8>) -> Result<Ino, i32> {
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
                name: name.to_owned(),
                composition_id: resp.composition_id,
                size,
            },
        );
        self.name_to_ino.insert(name.to_owned(), ino);
        Ok(ino)
    }

    /// Remove a file.
    pub fn unlink(&mut self, name: &str) -> Result<(), i32> {
        let ino = self.name_to_ino.remove(name).ok_or(libc_enoent())?;
        self.inodes.remove(&ino);
        Ok(())
    }

    /// List directory entries.
    pub fn readdir(&self) -> Vec<DirEntry> {
        let mut entries = vec![
            DirEntry {
                ino: 1,
                name: ".".into(),
                kind: FileKind::Directory,
            },
            DirEntry {
                ino: 1,
                name: "..".into(),
                kind: FileKind::Directory,
            },
        ];

        for (&ino, entry) in &self.inodes {
            match entry {
                InodeEntry::File { name, .. } | InodeEntry::Symlink { name, .. } => {
                    entries.push(DirEntry {
                        ino,
                        name: name.clone(),
                        kind: FileKind::Regular,
                    });
                }
                InodeEntry::Dir { name } => {
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

    /// Create a directory.
    pub fn mkdir(&mut self, name: &str) -> Result<Ino, i32> {
        if self.name_to_ino.contains_key(name) {
            return Err(17); // EEXIST
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, InodeEntry::Dir { name: name.to_owned() });
        self.name_to_ino.insert(name.to_owned(), ino);
        Ok(ino)
    }

    /// Remove a directory.
    pub fn rmdir(&mut self, name: &str) -> Result<(), i32> {
        let ino = self.name_to_ino.get(name).ok_or(libc_enoent())?;
        if !matches!(self.inodes.get(ino), Some(InodeEntry::Dir { .. })) {
            return Err(20); // ENOTDIR
        }
        let ino = self.name_to_ino.remove(name).unwrap();
        self.inodes.remove(&ino);
        Ok(())
    }

    /// Rename a file or directory.
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> Result<(), i32> {
        let ino = self.name_to_ino.remove(old_name).ok_or(libc_enoent())?;
        // Update the name in the inode entry.
        if let Some(entry) = self.inodes.get_mut(&ino) {
            match entry {
                InodeEntry::File { name, .. }
                | InodeEntry::Dir { name }
                | InodeEntry::Symlink { name, .. } => {
                    new_name.clone_into(name);
                }
                InodeEntry::Root => return Err(libc_eio()),
            }
        }
        // Remove any existing entry at new_name (overwrite semantics).
        if let Some(old_ino) = self.name_to_ino.remove(new_name) {
            self.inodes.remove(&old_ino);
        }
        self.name_to_ino.insert(new_name.to_owned(), ino);
        Ok(())
    }

    /// Set file attributes (mode only for now).
    pub fn setattr(&mut self, ino: Ino, _mode: Option<u32>) -> Result<FileAttr, i32> {
        // Attributes are computed, not stored (in-memory FS).
        // Return current attributes unchanged.
        self.getattr(ino)
    }

    /// Create a symbolic link.
    pub fn symlink(&mut self, name: &str, target: &str) -> Result<Ino, i32> {
        if self.name_to_ino.contains_key(name) {
            return Err(17); // EEXIST
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            InodeEntry::Symlink {
                name: name.to_owned(),
                target: target.to_owned(),
            },
        );
        self.name_to_ino.insert(name.to_owned(), ino);
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
}
