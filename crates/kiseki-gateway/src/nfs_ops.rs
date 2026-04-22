//! Shared NFS operations — used by both NFSv3 and NFSv4.2 dispatchers.
//!
//! Maps NFS file handles to compositions, provides stat/readdir, and
//! delegates read/write to `NfsGateway<GatewayOps>`.

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;
use crate::nfs::{NfsGateway, NfsReadRequest, NfsReadResponse, NfsWriteRequest, NfsWriteResponse};
use crate::nfs_dir::DirectoryIndex;
use crate::nfs_lock::LockManager;
use crate::ops::GatewayOps;

/// NFS file handle — 32-byte opaque identifier.
pub type FileHandle = [u8; 32];

/// File type for NFS attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
}

/// NFS file attributes (subset shared by v3 and v4).
#[derive(Debug, Clone)]
pub struct NfsAttrs {
    pub file_type: FileType,
    pub size: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub fileid: u64,
}

/// File handle registry — maps handles to namespace/composition IDs.
pub struct HandleRegistry {
    handles: Mutex<HashMap<FileHandle, HandleEntry>>,
    root_handles: Mutex<HashMap<NamespaceId, FileHandle>>,
}

#[derive(Clone, Debug)]
enum HandleEntry {
    Root {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
    },
    File {
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    },
}

impl HandleRegistry {
    pub fn new() -> Self {
        Self {
            handles: Mutex::new(HashMap::new()),
            root_handles: Mutex::new(HashMap::new()),
        }
    }

    /// Get or create a root directory handle for a namespace.
    pub fn root_handle(&self, namespace_id: NamespaceId, tenant_id: OrgId) -> FileHandle {
        let mut roots = self.root_handles.lock().unwrap();
        if let Some(&fh) = roots.get(&namespace_id) {
            return fh;
        }
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(namespace_id.0.as_bytes());
        fh[16] = 0xFF; // marker for root handle

        roots.insert(namespace_id, fh);
        self.handles.lock().unwrap().insert(
            fh,
            HandleEntry::Root {
                namespace_id,
                tenant_id,
            },
        );
        fh
    }

    /// Create a file handle for a composition.
    pub fn file_handle(
        &self,
        namespace_id: NamespaceId,
        tenant_id: OrgId,
        composition_id: CompositionId,
    ) -> FileHandle {
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(composition_id.0.as_bytes());
        self.handles.lock().unwrap().insert(
            fh,
            HandleEntry::File {
                namespace_id,
                tenant_id,
                composition_id,
            },
        );
        fh
    }

    /// Look up a handle. Returns `None` if not found.
    pub fn lookup(&self, fh: &FileHandle) -> Option<(NamespaceId, OrgId, Option<CompositionId>)> {
        let handles = self.handles.lock().unwrap();
        handles.get(fh).map(|entry| match entry {
            HandleEntry::Root {
                namespace_id,
                tenant_id,
            } => (*namespace_id, *tenant_id, None),
            HandleEntry::File {
                namespace_id,
                tenant_id,
                composition_id,
            } => (*namespace_id, *tenant_id, Some(*composition_id)),
        })
    }

    /// Check if a handle is a root directory.
    pub fn is_root(&self, fh: &FileHandle) -> bool {
        let handles = self.handles.lock().unwrap();
        matches!(handles.get(fh), Some(HandleEntry::Root { .. }))
    }
}

/// NFS operations context — wraps gateway + handle registry + lock manager.
pub struct NfsContext<G: GatewayOps> {
    pub gateway: NfsGateway<G>,
    pub handles: HandleRegistry,
    pub dir_index: DirectoryIndex,
    pub locks: LockManager,
    pub tenant_id: OrgId,
    pub namespace_id: NamespaceId,
}

impl<G: GatewayOps> NfsContext<G> {
    /// Create a new NFS context.
    pub fn new(gateway: NfsGateway<G>, tenant_id: OrgId, namespace_id: NamespaceId) -> Self {
        let handles = HandleRegistry::new();
        // Register root handle.
        handles.root_handle(namespace_id, tenant_id);

        Self {
            gateway,
            handles,
            dir_index: DirectoryIndex::new(),
            locks: LockManager::default(),
            tenant_id,
            namespace_id,
        }
    }

    /// Get attributes for a file handle.
    pub fn getattr(&self, fh: &FileHandle) -> Result<NfsAttrs, GatewayError> {
        if self.handles.is_root(fh) {
            return Ok(NfsAttrs {
                file_type: FileType::Directory,
                size: 4096,
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            });
        }

        let (_ns, _tenant, Some(comp_id)) = self
            .handles
            .lookup(fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale file handle".into()))?
        else {
            return Err(GatewayError::ProtocolError("expected file handle".into()));
        };

        // For now, return a fixed-size attr. Real implementation would
        // read composition metadata.
        Ok(NfsAttrs {
            file_type: FileType::Regular,
            size: 0, // unknown without reading
            mode: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            fileid: u64::from_le_bytes(comp_id.0.as_bytes()[..8].try_into().unwrap_or([0; 8])),
        })
    }

    /// Read from a file handle.
    pub fn read(
        &self,
        fh: &FileHandle,
        offset: u64,
        count: u32,
    ) -> Result<NfsReadResponse, GatewayError> {
        let (ns_id, tenant_id, Some(comp_id)) = self
            .handles
            .lookup(fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale file handle".into()))?
        else {
            return Err(GatewayError::ProtocolError(
                "cannot read a directory".into(),
            ));
        };

        self.gateway.read(NfsReadRequest {
            tenant_id,
            namespace_id: ns_id,
            composition_id: comp_id,
            offset,
            count,
        })
    }

    /// Write to create a new named file (NFS CREATE).
    pub fn write_named(
        &self,
        name: &str,
        data: Vec<u8>,
    ) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        let (fh, resp) = self.write(data)?;
        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            resp.composition_id,
            u64::from(resp.count),
        );
        Ok((fh, resp))
    }

    /// Write to create a new file (NFS CREATE + WRITE).
    pub fn write(&self, data: Vec<u8>) -> Result<(FileHandle, NfsWriteResponse), GatewayError> {
        let resp = self.gateway.write(NfsWriteRequest {
            tenant_id: self.tenant_id,
            namespace_id: self.namespace_id,
            data,
        })?;

        let fh = self
            .handles
            .file_handle(self.namespace_id, self.tenant_id, resp.composition_id);

        Ok((fh, resp))
    }

    /// Look up a file by name in the namespace. Returns handle + attrs.
    pub fn lookup_by_name(&self, name: &str) -> Option<(FileHandle, NfsAttrs)> {
        let entry = self.dir_index.lookup(self.namespace_id, name)?;
        let attrs = NfsAttrs {
            file_type: FileType::Regular,
            size: entry.size,
            mode: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            fileid: u64::from_le_bytes(entry.file_handle[..8].try_into().unwrap_or([0; 8])),
        };
        Some((entry.file_handle, attrs))
    }

    /// List directory entries for READDIR.
    pub fn readdir(&self) -> Vec<ReadDirEntry> {
        let mut entries = vec![
            ReadDirEntry {
                fileid: 1,
                name: ".".into(),
            },
            ReadDirEntry {
                fileid: 1,
                name: "..".into(),
            },
        ];

        for dir_entry in self.dir_index.list(self.namespace_id) {
            entries.push(ReadDirEntry {
                fileid: u64::from_le_bytes(dir_entry.file_handle[..8].try_into().unwrap_or([0; 8])),
                name: dir_entry.name,
            });
        }

        entries
    }

    /// Remove a file by name.
    pub fn remove_file(&self, name: &str) -> Result<(), GatewayError> {
        if self.dir_index.remove(self.namespace_id, name) {
            Ok(())
        } else {
            Err(GatewayError::ProtocolError("file not found".into()))
        }
    }

    /// Rename a file within the namespace.
    pub fn rename_file(&self, old_name: &str, new_name: &str) -> Result<(), GatewayError> {
        if self.dir_index.rename(self.namespace_id, old_name, new_name) {
            Ok(())
        } else {
            Err(GatewayError::ProtocolError("source file not found".into()))
        }
    }

    /// Set file attributes (mode, size). Returns updated attrs.
    pub fn setattr(&self, fh: &FileHandle, _mode: Option<u32>) -> Result<NfsAttrs, GatewayError> {
        // In-memory store: attrs are computed, not stored.
        // Return current attrs (mode update is advisory for now).
        self.getattr(fh)
    }

    /// Create a directory. Returns handle + attrs.
    pub fn mkdir(&self, name: &str) -> Result<(FileHandle, NfsAttrs), GatewayError> {
        // Use UUID v5 (deterministic hash of namespace + name) to avoid collisions.
        let dir_uuid = uuid::Uuid::new_v5(&self.namespace_id.0, name.as_bytes());
        let mut fh = [0u8; 32];
        fh[..16].copy_from_slice(dir_uuid.as_bytes());
        fh[16] = 0xFE; // marker for subdirectory

        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            CompositionId(uuid::Uuid::nil()), // dirs have no composition
            0,
        );

        Ok((
            fh,
            NfsAttrs {
                file_type: FileType::Directory,
                size: 4096,
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            },
        ))
    }

    /// Remove a directory by name.
    pub fn rmdir(&self, name: &str) -> Result<(), GatewayError> {
        if self.dir_index.remove(self.namespace_id, name) {
            Ok(())
        } else {
            Err(GatewayError::ProtocolError("directory not found".into()))
        }
    }

    /// Check access permissions. Returns allowed access bits.
    /// Single-tenant in-memory: all access granted.
    pub fn access(&self, fh: &FileHandle) -> Result<u32, GatewayError> {
        let _ = self
            .handles
            .lookup(fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale handle".into()))?;
        // ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_MODIFY | ACCESS4_EXTEND | ACCESS4_DELETE | ACCESS4_EXECUTE
        Ok(0x3F)
    }

    /// Create a symbolic link. Stores target as inline data.
    pub fn symlink(
        &self,
        name: &str,
        target: &str,
    ) -> Result<(FileHandle, NfsAttrs), GatewayError> {
        let (fh, resp) = self.write(target.as_bytes().to_vec())?;
        self.dir_index.insert(
            self.namespace_id,
            name.to_owned(),
            fh,
            resp.composition_id,
            target.len() as u64,
        );
        Ok((
            fh,
            NfsAttrs {
                file_type: FileType::Regular, // symlinks stored as regular files with link content
                size: target.len() as u64,
                mode: 0o777,
                nlink: 1,
                uid: 0,
                gid: 0,
                fileid: u64::from_le_bytes(fh[..8].try_into().unwrap_or([0; 8])),
            },
        ))
    }

    /// Read a symbolic link target. Capped at 4096 bytes (NFS3 MAXPATHLEN).
    pub fn readlink(&self, fh: &FileHandle) -> Result<String, GatewayError> {
        let resp = self.read(fh, 0, 4096)?;
        String::from_utf8(resp.data)
            .map_err(|_| GatewayError::ProtocolError("invalid symlink target".into()))
    }

    /// Create a hard link (within same namespace).
    pub fn link(&self, target_fh: &FileHandle, new_name: &str) -> Result<(), GatewayError> {
        let (ns, _tenant, comp_id) = self
            .handles
            .lookup(target_fh)
            .ok_or_else(|| GatewayError::ProtocolError("stale handle".into()))?;
        if ns != self.namespace_id {
            return Err(GatewayError::ProtocolError(
                "cross-namespace link (EXDEV)".into(),
            ));
        }
        self.dir_index.insert(
            self.namespace_id,
            new_name.to_owned(),
            *target_fh,
            comp_id.unwrap_or(CompositionId(uuid::Uuid::nil())),
            0,
        );
        Ok(())
    }

    /// Commit (fsync). No-op for in-memory; would flush redb for persistent.
    pub fn commit(&self) -> Result<(), GatewayError> {
        Ok(())
    }
}

/// Directory entry for READDIR response.
pub struct ReadDirEntry {
    pub fileid: u64,
    pub name: String,
}
