//! pNFS (parallel NFS) layout support.
//!
//! Implements file-level layout delegation per RFC 5661/5663.
//! Clients can perform direct I/O to storage devices after
//! obtaining a layout from the metadata server.
//!
//! ADR-038 introduces a Flexible Files Layout (RFC 8435) replacement
//! for the legacy in-memory `LayoutManager` below. The new types
//! ([`PnfsFileHandle`] and friends) live in this module from Phase 15a
//! onwards. The legacy types are retained until Phase 15b replaces
//! `op_layoutget` with the new flow.

use aws_lc_rs::{constant_time, hmac};
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

// =============================================================================
// pNFS File Handle (ADR-038 §D4.3)
// =============================================================================

/// Domain-separation tag prepended to the MAC input.
/// Spec: ADR-038 §D4.3.
pub const PNFS_FH_MAC_DOMAIN: &[u8] = b"kiseki/pnfs-fh/v1\x00";

/// Wire size of an encoded `PnfsFileHandle`: 60-byte payload + 16-byte MAC.
/// RFC 5661 §5 `NFS4_FHSIZE` max is 128 — well under the cap.
pub const PNFS_FH_BYTES: usize = 76;

/// Size of the unsigned payload (everything except the MAC).
pub const PNFS_FH_PAYLOAD_BYTES: usize = 60;

/// MAC truncation length per ADR-038 §D4.3 (NIST SP 800-107 §5.1).
pub const PNFS_FH_MAC_BYTES: usize = 16;

/// pNFS-DS file handle. Self-authenticating: the MDS constructs it
/// at LAYOUTGET time with a MAC over its fields; each DS validates
/// the MAC on every op. Stateless on the DS side (I-PN2).
///
/// Spec: ADR-038 §D4 (auth, including §D4.3 wire encoding),
/// §D5 (encryption boundary).
/// Invariants: I-PN1, I-PN2, I-PN3.
///
/// Wire layout (76 bytes total, big-endian for integers, raw UUID
/// bytes for IDs):
///
/// ```text
///   offset  size  field
///        0    16  tenant_id        (uuid::Uuid bytes)
///       16    16  namespace_id     (uuid::Uuid bytes)
///       32    16  composition_id   (uuid::Uuid bytes)
///       48     4  stripe_index     (u32 BE)
///       52     8  expiry_ms        (u64 BE, ms since Unix epoch)
///       60    16  mac              (HMAC-SHA256(K_layout, ...) truncated to 16)
/// ```
///
/// MAC input is `b"kiseki/pnfs-fh/v1\0" || bytes[0..60]`. The
/// domain-separation tag prevents cross-purpose use of `K_layout`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PnfsFileHandle {
    /// Owning tenant — bound into the MAC so cross-tenant fh4
    /// substitution fails MAC verification.
    pub tenant_id: OrgId,
    /// Namespace the layout was issued for.
    pub namespace_id: NamespaceId,
    /// Composition this stripe references.
    pub composition_id: CompositionId,
    /// 0-based stripe index within the composition.
    pub stripe_index: u32,
    /// Wall-clock expiry as ms since Unix epoch. DS rejects after this.
    pub expiry_ms: u64,
    /// Truncated HMAC-SHA256 over the domain-separated payload.
    pub mac: [u8; PNFS_FH_MAC_BYTES],
}

/// `K_layout`: per-cluster MAC key for fh4 authentication.
///
/// Spec: ADR-038 §D4.1 — `K_layout = HKDF-SHA256(master_key,
/// salt=cluster_id_bytes, info=b"kiseki/pnfs-fh/v1")`.
///
/// Wrapped to make accidental copies surface in code review.
#[derive(Clone)]
pub struct PnfsFhMacKey(zeroize::Zeroizing<[u8; 32]>);

impl PnfsFhMacKey {
    /// Construct from raw bytes (test seam; production uses
    /// [`derive_pnfs_fh_mac_key`]).
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(zeroize::Zeroizing::new(bytes))
    }

    fn material(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Derive `K_layout` from the cluster master key + cluster id.
///
/// Spec: ADR-038 §D4.1.
#[must_use]
pub fn derive_pnfs_fh_mac_key(
    master_key: &[u8; 32],
    cluster_id: &[u8; 16],
) -> PnfsFhMacKey {
    use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};

    let salt = Salt::new(HKDF_SHA256, cluster_id);
    let prk = salt.extract(master_key);

    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    let okm = prk
        .expand(&[PNFS_FH_MAC_DOMAIN], HkdfLen32)
        .expect("HKDF-SHA256 expand of 32 bytes is always within length bounds");
    okm.fill(&mut *out)
        .expect("HKDF fill into a 32-byte buffer cannot fail");
    PnfsFhMacKey(out)
}

struct HkdfLen32;
impl aws_lc_rs::hkdf::KeyType for HkdfLen32 {
    fn len(&self) -> usize {
        32
    }
}

impl PnfsFileHandle {
    /// Build a `PnfsFileHandle` and compute its MAC.
    /// Used by the MDS at LAYOUTGET time.
    #[must_use]
    pub fn issue(
        key: &PnfsFhMacKey,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        composition_id: CompositionId,
        stripe_index: u32,
        expiry_ms: u64,
    ) -> Self {
        let payload = encode_payload(
            tenant_id,
            namespace_id,
            composition_id,
            stripe_index,
            expiry_ms,
        );
        let mac = compute_mac(key, &payload);
        Self {
            tenant_id,
            namespace_id,
            composition_id,
            stripe_index,
            expiry_ms,
            mac,
        }
    }

    /// Encode this handle into its on-the-wire 76-byte representation.
    #[must_use]
    pub fn encode(&self) -> [u8; PNFS_FH_BYTES] {
        let payload = encode_payload(
            self.tenant_id,
            self.namespace_id,
            self.composition_id,
            self.stripe_index,
            self.expiry_ms,
        );
        let mut out = [0u8; PNFS_FH_BYTES];
        out[..PNFS_FH_PAYLOAD_BYTES].copy_from_slice(&payload);
        out[PNFS_FH_PAYLOAD_BYTES..].copy_from_slice(&self.mac);
        out
    }

    /// Decode from on-the-wire bytes. Length-checked only. The MAC
    /// is parsed but **not** validated — callers must call
    /// [`PnfsFileHandle::validate`] before honoring the handle.
    pub fn decode(bytes: &[u8]) -> Result<Self, FhDecodeError> {
        if bytes.len() != PNFS_FH_BYTES {
            return Err(FhDecodeError::WrongLength {
                expected: PNFS_FH_BYTES,
                got: bytes.len(),
            });
        }
        let tenant_id = OrgId(uuid_from_slice(&bytes[0..16]));
        let namespace_id = NamespaceId(uuid_from_slice(&bytes[16..32]));
        let composition_id = CompositionId(uuid_from_slice(&bytes[32..48]));
        let stripe_index =
            u32::from_be_bytes(bytes[48..52].try_into().expect("4 bytes"));
        let expiry_ms = u64::from_be_bytes(bytes[52..60].try_into().expect("8 bytes"));
        let mut mac = [0u8; PNFS_FH_MAC_BYTES];
        mac.copy_from_slice(&bytes[PNFS_FH_PAYLOAD_BYTES..]);
        Ok(Self {
            tenant_id,
            namespace_id,
            composition_id,
            stripe_index,
            expiry_ms,
            mac,
        })
    }

    /// Validate that:
    /// 1. the MAC matches `key`, AND
    /// 2. `expiry_ms > now_ms`.
    ///
    /// Constant-time MAC compare per I-PN1.
    pub fn validate(&self, key: &PnfsFhMacKey, now_ms: u64) -> Result<(), FhValidateError> {
        let payload = encode_payload(
            self.tenant_id,
            self.namespace_id,
            self.composition_id,
            self.stripe_index,
            self.expiry_ms,
        );
        let expected = compute_mac(key, &payload);
        if constant_time::verify_slices_are_equal(&self.mac, &expected).is_err() {
            return Err(FhValidateError::MacMismatch);
        }
        if self.expiry_ms <= now_ms {
            return Err(FhValidateError::Expired {
                expiry_ms: self.expiry_ms,
                now_ms,
            });
        }
        Ok(())
    }
}

fn encode_payload(
    tenant_id: OrgId,
    namespace_id: NamespaceId,
    composition_id: CompositionId,
    stripe_index: u32,
    expiry_ms: u64,
) -> [u8; PNFS_FH_PAYLOAD_BYTES] {
    let mut out = [0u8; PNFS_FH_PAYLOAD_BYTES];
    out[0..16].copy_from_slice(tenant_id.0.as_bytes());
    out[16..32].copy_from_slice(namespace_id.0.as_bytes());
    out[32..48].copy_from_slice(composition_id.0.as_bytes());
    out[48..52].copy_from_slice(&stripe_index.to_be_bytes());
    out[52..60].copy_from_slice(&expiry_ms.to_be_bytes());
    out
}

fn compute_mac(key: &PnfsFhMacKey, payload: &[u8; PNFS_FH_PAYLOAD_BYTES]) -> [u8; PNFS_FH_MAC_BYTES] {
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, key.material());
    let mut ctx = hmac::Context::with_key(&hmac_key);
    ctx.update(PNFS_FH_MAC_DOMAIN);
    ctx.update(payload);
    let tag = ctx.sign();
    let mut out = [0u8; PNFS_FH_MAC_BYTES];
    out.copy_from_slice(&tag.as_ref()[..PNFS_FH_MAC_BYTES]);
    out
}

fn uuid_from_slice(b: &[u8]) -> uuid::Uuid {
    let mut arr = [0u8; 16];
    arr.copy_from_slice(b);
    uuid::Uuid::from_bytes(arr)
}

/// Failure modes from [`PnfsFileHandle::decode`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FhDecodeError {
    /// Wire-format length did not match `PNFS_FH_BYTES`.
    #[error("expected {expected}-byte fh4, got {got}")]
    WrongLength {
        /// Required size (`PNFS_FH_BYTES` = 76).
        expected: usize,
        /// Length actually received from the wire.
        got: usize,
    },
}

/// Failure modes from [`PnfsFileHandle::validate`]. Callers translate
/// either variant into `NFS4ERR_BADHANDLE` on the wire (I-PN1).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FhValidateError {
    /// HMAC over the payload did not match the per-cluster MAC key.
    /// Indicates forgery or a key-rotation gap.
    #[error("MAC mismatch — fh4 forged or computed under a different key")]
    MacMismatch,
    /// Wall-clock expiry has passed (or exactly matches `now_ms`).
    #[error("fh4 expired (expiry_ms={expiry_ms}, now_ms={now_ms})")]
    Expired {
        /// Expiry stamp encoded in the fh4.
        expiry_ms: u64,
        /// Current wall-clock used by the validator.
        now_ms: u64,
    },
}

// =============================================================================
// Legacy LayoutManager (replaced in Phase 15b)
// =============================================================================


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
mod fh_tests {
    use super::*;

    fn fixed_key() -> PnfsFhMacKey {
        PnfsFhMacKey::from_bytes([0xab; 32])
    }

    fn fixed_handle(expiry_ms: u64) -> PnfsFileHandle {
        PnfsFileHandle::issue(
            &fixed_key(),
            OrgId(uuid::Uuid::from_bytes([0x11; 16])),
            NamespaceId(uuid::Uuid::from_bytes([0x22; 16])),
            CompositionId(uuid::Uuid::from_bytes([0x33; 16])),
            42,
            expiry_ms,
        )
    }

    #[test]
    fn fh_const_sizes_match_spec() {
        assert_eq!(PNFS_FH_BYTES, 76);
        assert_eq!(PNFS_FH_PAYLOAD_BYTES, 60);
        assert_eq!(PNFS_FH_MAC_BYTES, 16);
        assert_eq!(PNFS_FH_PAYLOAD_BYTES + PNFS_FH_MAC_BYTES, PNFS_FH_BYTES);
    }

    #[test]
    fn encode_then_decode_roundtrips_all_fields() {
        let h = fixed_handle(1_000_000);
        let bytes = h.encode();
        assert_eq!(bytes.len(), PNFS_FH_BYTES);
        let back = PnfsFileHandle::decode(&bytes).expect("decode");
        assert_eq!(back, h);
    }

    #[test]
    fn decode_wrong_length_rejected() {
        let err = PnfsFileHandle::decode(&[0u8; 75]).unwrap_err();
        assert_eq!(
            err,
            FhDecodeError::WrongLength {
                expected: 76,
                got: 75,
            }
        );
    }

    #[test]
    fn validate_succeeds_with_correct_key_and_future_expiry() {
        let h = fixed_handle(u64::MAX);
        h.validate(&fixed_key(), 0).expect("valid");
    }

    #[test]
    fn validate_rejects_wrong_key_with_mac_mismatch() {
        let h = fixed_handle(u64::MAX);
        let other_key = PnfsFhMacKey::from_bytes([0xcd; 32]);
        let err = h.validate(&other_key, 0).unwrap_err();
        assert_eq!(err, FhValidateError::MacMismatch);
    }

    #[test]
    fn validate_rejects_tampered_payload_byte() {
        let h = fixed_handle(u64::MAX);
        let mut bytes = h.encode();
        bytes[10] ^= 0x01; // flip a bit inside tenant_id
        let tampered = PnfsFileHandle::decode(&bytes).expect("decode");
        let err = tampered.validate(&fixed_key(), 0).unwrap_err();
        assert_eq!(err, FhValidateError::MacMismatch);
    }

    #[test]
    fn validate_rejects_expired_fh() {
        let h = fixed_handle(1_000); // expiry_ms = 1000
        let err = h.validate(&fixed_key(), 5_000).unwrap_err();
        assert_eq!(
            err,
            FhValidateError::Expired {
                expiry_ms: 1_000,
                now_ms: 5_000,
            }
        );
    }

    #[test]
    fn validate_rejects_at_exact_expiry_boundary() {
        // ADR-038 §D4.4 wording: `expiry_ms > now_ms` — equality is expired.
        let h = fixed_handle(5_000);
        assert!(matches!(
            h.validate(&fixed_key(), 5_000),
            Err(FhValidateError::Expired { .. })
        ));
    }

    #[test]
    fn derive_pnfs_fh_mac_key_is_deterministic() {
        let master = [0x42; 32];
        let cluster = [0x77; 16];
        let k1 = derive_pnfs_fh_mac_key(&master, &cluster);
        let k2 = derive_pnfs_fh_mac_key(&master, &cluster);
        assert_eq!(*k1.material(), *k2.material());
    }

    #[test]
    fn derive_pnfs_fh_mac_key_differs_per_cluster_id() {
        let master = [0x42; 32];
        let k_a = derive_pnfs_fh_mac_key(&master, &[0x01; 16]);
        let k_b = derive_pnfs_fh_mac_key(&master, &[0x02; 16]);
        assert_ne!(*k_a.material(), *k_b.material());
    }

    #[test]
    fn derive_pnfs_fh_mac_key_differs_per_master_key() {
        let cluster = [0x77; 16];
        let k_a = derive_pnfs_fh_mac_key(&[0x01; 32], &cluster);
        let k_b = derive_pnfs_fh_mac_key(&[0x02; 32], &cluster);
        assert_ne!(*k_a.material(), *k_b.material());
    }

    #[test]
    fn fh_uses_domain_separation_tag() {
        // If the MAC input were just the payload (no PNFS_FH_MAC_DOMAIN
        // prefix), this round-trip with a manually-computed
        // hmac-without-tag would validate. Asserting that it does NOT
        // pins the domain-separation requirement.
        use aws_lc_rs::hmac;
        let key = fixed_key();
        let h = fixed_handle(u64::MAX);
        let payload = encode_payload(
            h.tenant_id,
            h.namespace_id,
            h.composition_id,
            h.stripe_index,
            h.expiry_ms,
        );

        let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, key.material());
        let raw_mac_no_domain = hmac::sign(&hmac_key, &payload);
        let mut forged_mac = [0u8; PNFS_FH_MAC_BYTES];
        forged_mac.copy_from_slice(&raw_mac_no_domain.as_ref()[..PNFS_FH_MAC_BYTES]);

        let mut forged = h.clone();
        forged.mac = forged_mac;
        assert_eq!(
            forged.validate(&key, 0).unwrap_err(),
            FhValidateError::MacMismatch
        );
    }

    #[test]
    fn fh_payload_field_widths_total_60() {
        // Sentinel test: if anyone changes ID widths or removes a field
        // they must update ADR-038 §D4.3 and this test together.
        let h = fixed_handle(0);
        let payload = encode_payload(
            h.tenant_id,
            h.namespace_id,
            h.composition_id,
            h.stripe_index,
            h.expiry_ms,
        );
        assert_eq!(payload.len(), 60);
        // First 16 bytes match raw uuid bytes.
        assert_eq!(&payload[0..16], h.tenant_id.0.as_bytes());
        // stripe_index at offset 48, big-endian.
        let stripe_be = &payload[48..52];
        assert_eq!(
            u32::from_be_bytes(stripe_be.try_into().unwrap()),
            h.stripe_index
        );
    }

    #[test]
    fn issue_then_validate_with_same_key_succeeds() {
        let key = fixed_key();
        let h = PnfsFileHandle::issue(
            &key,
            OrgId(uuid::Uuid::from_bytes([0x11; 16])),
            NamespaceId(uuid::Uuid::from_bytes([0x22; 16])),
            CompositionId(uuid::Uuid::from_bytes([0x33; 16])),
            7,
            u64::MAX,
        );
        h.validate(&key, 0).expect("valid");
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
