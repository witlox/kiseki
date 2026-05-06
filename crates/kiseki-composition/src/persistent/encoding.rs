//! On-disk record encoding shared by every `CompositionStorage`
//! backend. Lives outside any specific backend module so the wire
//! format is the single source of truth — swapping backends does not
//! re-serialize rows.
//!
//! Schema version bumps go through ADR-040 §D8.

use kiseki_common::ids::{NamespaceId, SequenceNumber};

use super::error::PersistentStoreError;
use crate::composition::Composition;

/// Current on-disk schema version. Bumped on incompatible changes.
pub const COMPOSITION_RECORD_SCHEMA_VERSION: u8 = 1;

// -- Composition record (`comps` partition) ---------------------------

/// `[1 byte: version][postcard payload]` — ADR-040 §D2 layout.
pub fn encode_composition(comp: &Composition) -> Result<Vec<u8>, PersistentStoreError> {
    let mut out = Vec::with_capacity(280);
    out.push(COMPOSITION_RECORD_SCHEMA_VERSION);
    let payload = postcard::to_stdvec(comp)?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Inverse of [`encode_composition`]. Rejects records whose
/// `schema_version` byte is greater than what this binary supports.
pub fn decode_composition(bytes: &[u8]) -> Result<Composition, PersistentStoreError> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(PersistentStoreError::Decode("empty record".to_owned()));
    };
    if version > COMPOSITION_RECORD_SCHEMA_VERSION {
        return Err(PersistentStoreError::SchemaTooNew {
            found: version,
            supported: COMPOSITION_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(postcard::from_bytes(payload)?)
}

// -- Name index keys (`names` / `names_rev` partitions) ---------------

/// Encode a (`namespace_id`, name) tuple as a flat key. Layout:
/// 16 bytes `ns_id` || UTF-8 name. The fixed namespace prefix gives a
/// free per-namespace range scan in any LSM / B-tree backend.
pub fn name_key(ns: NamespaceId, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + name.len());
    out.extend_from_slice(ns.0.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// Decode the reverse-index value (composite (ns, name)) back into
/// the typed pair. Mirror of [`name_key`].
pub fn decode_name_key(bytes: &[u8]) -> Result<(NamespaceId, String), String> {
    if bytes.len() < 16 {
        return Err(format!("name key too short: {}", bytes.len()));
    }
    let mut ns_buf = [0u8; 16];
    ns_buf.copy_from_slice(&bytes[..16]);
    let name = std::str::from_utf8(&bytes[16..])
        .map_err(|e| format!("name utf8: {e}"))?
        .to_owned();
    Ok((NamespaceId(uuid::Uuid::from_bytes(ns_buf)), name))
}

// -- Stuck-state meta value -------------------------------------------

/// Empty bytes = `None` (not stuck). 12 bytes = `Some((seq u64 LE,
/// retries u32 LE))`. Any other length is malformed.
#[must_use]
pub fn encode_stuck_state(state: Option<(SequenceNumber, u32)>) -> Vec<u8> {
    match state {
        None => Vec::new(),
        Some((seq, retries)) => {
            let mut out = Vec::with_capacity(12);
            out.extend_from_slice(&seq.0.to_le_bytes());
            out.extend_from_slice(&retries.to_le_bytes());
            out
        }
    }
}

/// Inverse of [`encode_stuck_state`]. Returns the malformed-length
/// case as `Decode` so the metric label matches the schema-mismatch
/// taxonomy.
pub fn decode_stuck_state(
    bytes: &[u8],
) -> Result<Option<(SequenceNumber, u32)>, PersistentStoreError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 12 {
        return Err(PersistentStoreError::Decode(format!(
            "stuck_state has wrong length: {}",
            bytes.len()
        )));
    }
    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&bytes[0..8]);
    let mut retry_bytes = [0u8; 4];
    retry_bytes.copy_from_slice(&bytes[8..12]);
    Ok(Some((
        SequenceNumber(u64::from_le_bytes(seq_bytes)),
        u32::from_le_bytes(retry_bytes),
    )))
}
