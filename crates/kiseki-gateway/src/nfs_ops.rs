//! Shared NFS operations — used by both NFSv3 and NFSv4.2 dispatchers.
//!
//! Maps NFS file handles to compositions, provides stat/readdir, and
//! delegates read/write to `NfsGateway<GatewayOps>`.

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::error::GatewayError;
use crate::nfs::{NfsGateway, NfsReadRequest, NfsReadResponse, NfsWriteRequest, NfsWriteResponse};
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

/// NFS operations context — wraps gateway + handle registry.
pub struct NfsContext<G: GatewayOps> {
    pub gateway: NfsGateway<G>,
    pub handles: HandleRegistry,
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
}
