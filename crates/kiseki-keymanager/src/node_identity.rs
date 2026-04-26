//! Node identity → at-rest encryption key (Phase 14e, ADR-016 derived).
//!
//! Per the Phase 14 plan (decision 14e), every node derives its
//! at-rest encryption key from a per-node identity. This module
//! defines the [`NodeIdentitySource`] trait and four implementations
//! with a documented selection precedence:
//!
//! 1. [`SpiffeIdentitySource`] — SPIFFE/SPIRE workload SVID. Used in
//!    production when `KISEKI_SPIFFE_SOCKET` is set. (The current
//!    implementation reads the SVID private key from a path on disk;
//!    a real Workload API client is a follow-up.)
//! 2. [`MtlsIdentitySource`] — **default**. Derives from the node's
//!    existing mTLS private key (the cert/key already loaded for the
//!    cluster-CA data fabric — A-T-2 / I-Auth1). No new infrastructure
//!    required; works in every cluster that already has mTLS.
//! 3. [`FileIdentitySource`] — random per-node key in
//!    `<data_dir>/node-identity.key` (mode 0600 on unix, auto-generated
//!    on first boot). Single-node dev mode and any cluster that runs
//!    in plaintext (no mTLS configured).
//! 4. [`TestIdentitySource`] — raw bytes for unit and BDD tests. Never
//!    wired into the runtime.
//!
//! All four feed `HKDF-SHA256(secret, salt=node_id, info="kiseki/at-rest/v1")`
//! before the bytes are used. The `info` string domain-separates the
//! derived key from any other purpose the source secret might serve
//! (mTLS handshakes, SVID signatures, etc.).

use std::path::{Path, PathBuf};

use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};
use zeroize::Zeroizing;

/// HKDF info string — versioned for crypto-agility. Any future change
/// to the derivation must bump the version and add a re-wrap migration.
const HKDF_INFO: &[u8] = b"kiseki/at-rest/v1";

/// Errors a [`NodeIdentitySource`] can surface.
#[derive(Debug, thiserror::Error)]
pub enum NodeIdentityError {
    /// Source secret could not be loaded (file missing, socket unreachable, etc.).
    #[error("node identity unavailable: {0}")]
    Unavailable(String),
    /// HKDF derivation failed.
    #[error("HKDF derivation failed")]
    HkdfFailed,
    /// I/O error from the filesystem source.
    #[error("node identity I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Source of bytes that uniquely identifies *this node*.
///
/// The exposed bytes are passed through HKDF before any production use
/// — see [`derive_at_rest_key`]. Implementations should never return
/// the same bytes for two different nodes (otherwise their at-rest
/// stores would be cross-readable).
pub trait NodeIdentitySource: Send + Sync {
    /// Yield the source bytes (e.g. raw private key bytes). Wrapped
    /// in [`Zeroizing`] so the buffer is scrubbed when the caller
    /// drops it.
    fn source_secret(&self) -> Result<Zeroizing<Vec<u8>>, NodeIdentityError>;

    /// Short identifier for the source kind, suitable for logs.
    fn kind(&self) -> &'static str;
}

/// Derive a 32-byte at-rest encryption key from the node identity.
///
/// `salt` should be the node id (or any per-node value) so two nodes
/// with the same source never share the derived key. `info` is the
/// versioned domain-separator constant.
pub fn derive_at_rest_key<S: NodeIdentitySource + ?Sized>(
    source: &S,
    salt: &[u8],
) -> Result<Zeroizing<[u8; 32]>, NodeIdentityError> {
    let secret = source.source_secret()?;
    let salt = Salt::new(HKDF_SHA256, salt);
    let prk = salt.extract(&secret);

    let mut out = Zeroizing::new([0u8; 32]);
    prk.expand(&[HKDF_INFO], HkdfLen)
        .and_then(|okm| okm.fill(&mut *out))
        .map_err(|_| NodeIdentityError::HkdfFailed)?;
    Ok(out)
}

struct HkdfLen;
impl aws_lc_rs::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        32
    }
}

// ---------------------------------------------------------------------------
// Implementations
// ---------------------------------------------------------------------------

/// SPIFFE/SPIRE-backed identity source. Used in production when
/// `KISEKI_SPIFFE_SOCKET` is set.
///
/// Current implementation reads the SVID private key from a path on
/// disk — sufficient for the trait + tests. A real Workload API client
/// (Unix socket + gRPC to spire-agent) is a follow-up; the trait
/// surface stays the same.
pub struct SpiffeIdentitySource {
    svid_key_path: PathBuf,
}

impl SpiffeIdentitySource {
    /// Build from an absolute path to the SVID private key.
    #[must_use]
    pub fn new(svid_key_path: PathBuf) -> Self {
        Self { svid_key_path }
    }
}

impl NodeIdentitySource for SpiffeIdentitySource {
    fn source_secret(&self) -> Result<Zeroizing<Vec<u8>>, NodeIdentityError> {
        let bytes = std::fs::read(&self.svid_key_path)
            .map_err(|e| NodeIdentityError::Unavailable(format!("SVID key: {e}")))?;
        Ok(Zeroizing::new(bytes))
    }

    fn kind(&self) -> &'static str {
        "spiffe"
    }
}

/// mTLS-derived identity source. Reads the node's existing TLS
/// private key — the same key already loaded for the data-fabric
/// mTLS handshake (cluster CA, A-T-2 / I-Auth1).
///
/// This is the default in any cluster that has mTLS configured (which
/// is every production cluster per the assumptions matrix).
pub struct MtlsIdentitySource {
    key_pem_path: PathBuf,
}

impl MtlsIdentitySource {
    /// Build from the path to the node's TLS private key (PEM file).
    #[must_use]
    pub fn new(key_pem_path: PathBuf) -> Self {
        Self { key_pem_path }
    }
}

impl NodeIdentitySource for MtlsIdentitySource {
    fn source_secret(&self) -> Result<Zeroizing<Vec<u8>>, NodeIdentityError> {
        let bytes = std::fs::read(&self.key_pem_path)
            .map_err(|e| NodeIdentityError::Unavailable(format!("mTLS key: {e}")))?;
        Ok(Zeroizing::new(bytes))
    }

    fn kind(&self) -> &'static str {
        "mtls"
    }
}

/// File-based identity source. Reads (or, on first call, generates)
/// a 32-byte random key at the given path with mode 0600 on unix.
///
/// Used for single-node dev mode and any cluster that runs in
/// plaintext (no mTLS configured). Provides node binding without an
/// external service.
pub struct FileIdentitySource {
    key_path: PathBuf,
}

impl FileIdentitySource {
    /// Build from the path to the node-identity key file. Auto-creates
    /// the file with random bytes (and mode 0600 on unix) if missing.
    pub fn new(key_path: PathBuf) -> Result<Self, NodeIdentityError> {
        let s = Self { key_path };
        s.ensure_exists()?;
        Ok(s)
    }

    fn ensure_exists(&self) -> Result<(), NodeIdentityError> {
        if self.key_path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = [0u8; 32];
        aws_lc_rs::rand::fill(&mut bytes)
            .map_err(|_| NodeIdentityError::Unavailable("CSPRNG".into()))?;
        std::fs::write(&self.key_path, bytes)?;
        // Mode 0600 on unix — owner read/write only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&self.key_path, perms)?;
        }
        Ok(())
    }
}

impl NodeIdentitySource for FileIdentitySource {
    fn source_secret(&self) -> Result<Zeroizing<Vec<u8>>, NodeIdentityError> {
        let bytes = std::fs::read(&self.key_path)?;
        Ok(Zeroizing::new(bytes))
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

/// Test-only source with raw bytes. Never wired into the runtime —
/// the runtime selection precedence (`select_node_identity`) only
/// returns the first three.
pub struct TestIdentitySource {
    bytes: Vec<u8>,
}

impl TestIdentitySource {
    /// Build from raw bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl NodeIdentitySource for TestIdentitySource {
    fn source_secret(&self) -> Result<Zeroizing<Vec<u8>>, NodeIdentityError> {
        Ok(Zeroizing::new(self.bytes.clone()))
    }

    fn kind(&self) -> &'static str {
        "test"
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Inputs the runtime can supply to select an appropriate source.
pub struct NodeIdentityInputs<'a> {
    /// Path from `KISEKI_SPIFFE_SOCKET` env var (or `None` if unset).
    /// Today this is treated as a path to the SVID private key on disk;
    /// a real Workload API integration is a follow-up.
    pub spiffe_path: Option<&'a Path>,
    /// Path to the node's mTLS private key (`KISEKI_KEY_PATH`), or `None`.
    pub mtls_key_path: Option<&'a Path>,
    /// Data dir for the file-based fallback (`KISEKI_DATA_DIR`), or `None`.
    pub data_dir: Option<&'a Path>,
}

/// Pick the highest-precedence available source. Returns `None` if
/// none of the inputs satisfy any source — caller should reject the
/// startup configuration in that case.
#[must_use]
pub fn select_node_identity(
    inputs: &NodeIdentityInputs<'_>,
) -> Option<Box<dyn NodeIdentitySource>> {
    if let Some(p) = inputs.spiffe_path {
        return Some(Box::new(SpiffeIdentitySource::new(p.to_path_buf())));
    }
    if let Some(p) = inputs.mtls_key_path {
        return Some(Box::new(MtlsIdentitySource::new(p.to_path_buf())));
    }
    if let Some(d) = inputs.data_dir {
        let path = d.join("node-identity.key");
        if let Ok(src) = FileIdentitySource::new(path) {
            return Some(Box::new(src));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_round_trips_and_kind() {
        let src = TestIdentitySource::new(vec![0xab; 64]);
        assert_eq!(src.kind(), "test");
        assert_eq!(src.source_secret().unwrap().as_slice(), &vec![0xab; 64][..]);
    }

    #[test]
    fn derive_is_deterministic_and_salt_dependent() {
        let src = TestIdentitySource::new(vec![0x11; 32]);
        let k1 = derive_at_rest_key(&src, b"node-1").unwrap();
        let k2 = derive_at_rest_key(&src, b"node-1").unwrap();
        assert_eq!(*k1, *k2, "same salt → same key");
        let k3 = derive_at_rest_key(&src, b"node-2").unwrap();
        assert_ne!(*k1, *k3, "different salt → different key");
    }

    #[test]
    fn derive_changes_with_source_secret() {
        let a = TestIdentitySource::new(vec![0x11; 32]);
        let b = TestIdentitySource::new(vec![0x22; 32]);
        let ka = derive_at_rest_key(&a, b"node-1").unwrap();
        let kb = derive_at_rest_key(&b, b"node-1").unwrap();
        assert_ne!(*ka, *kb);
    }

    #[test]
    fn file_source_auto_creates_with_random_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-identity.key");
        assert!(!path.exists());
        let src = FileIdentitySource::new(path.clone()).unwrap();
        assert!(path.exists());
        let bytes = src.source_secret().unwrap();
        assert_eq!(bytes.len(), 32);
        // Re-opening returns the same bytes.
        let src2 = FileIdentitySource::new(path).unwrap();
        assert_eq!(*bytes, *src2.source_secret().unwrap());
    }

    #[test]
    fn file_source_honours_mode_0600_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("node-identity.key");
            let _ = FileIdentitySource::new(path.clone()).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "node-identity.key must be 0600");
        }
    }

    #[test]
    fn mtls_source_reads_pem_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.key");
        std::fs::write(&path, b"-----BEGIN PRIVATE KEY-----\nfake\n").unwrap();
        let src = MtlsIdentitySource::new(path);
        assert_eq!(src.kind(), "mtls");
        assert!(src.source_secret().unwrap().starts_with(b"-----BEGIN"));
    }

    #[test]
    fn spiffe_source_reads_svid_key_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svid.key");
        std::fs::write(&path, b"svid-private-key-bytes").unwrap();
        let src = SpiffeIdentitySource::new(path);
        assert_eq!(src.kind(), "spiffe");
        assert_eq!(
            src.source_secret().unwrap().as_slice(),
            b"svid-private-key-bytes"
        );
    }

    #[test]
    fn select_prefers_spiffe_over_mtls_over_file() {
        let dir = tempfile::tempdir().unwrap();
        let svid = dir.path().join("svid.key");
        let mtls = dir.path().join("mtls.key");
        std::fs::write(&svid, b"x").unwrap();
        std::fs::write(&mtls, b"y").unwrap();

        let only_file = select_node_identity(&NodeIdentityInputs {
            spiffe_path: None,
            mtls_key_path: None,
            data_dir: Some(dir.path()),
        })
        .unwrap();
        assert_eq!(only_file.kind(), "file");

        let mtls_over_file = select_node_identity(&NodeIdentityInputs {
            spiffe_path: None,
            mtls_key_path: Some(&mtls),
            data_dir: Some(dir.path()),
        })
        .unwrap();
        assert_eq!(mtls_over_file.kind(), "mtls");

        let spiffe_over_all = select_node_identity(&NodeIdentityInputs {
            spiffe_path: Some(&svid),
            mtls_key_path: Some(&mtls),
            data_dir: Some(dir.path()),
        })
        .unwrap();
        assert_eq!(spiffe_over_all.kind(), "spiffe");
    }

    #[test]
    fn select_returns_none_when_no_inputs_available() {
        let chosen = select_node_identity(&NodeIdentityInputs {
            spiffe_path: None,
            mtls_key_path: None,
            data_dir: None,
        });
        assert!(chosen.is_none());
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let src = MtlsIdentitySource::new(PathBuf::from("/nonexistent/path"));
        let err = src.source_secret().unwrap_err();
        assert!(matches!(err, NodeIdentityError::Unavailable(_)));
    }
}
