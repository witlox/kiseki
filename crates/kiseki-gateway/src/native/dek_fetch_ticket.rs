//! DEK-fetch tickets for `TrustedCompute` namespaces (ADR-042 §8).
//!
//! Wire format mirrors `handle_token`:
//!
//! ```text
//! [1 byte schema_version][postcard(DekFetchTicketInner)][32 byte HMAC tag]
//! ```
//!
//! HMAC keyed by `EpochKeys::dek_fetch_ticket`. Each ticket commits to
//! `(tenant, namespace, composition, chunk_id, crypto_boundary_at_read,
//! expires_at, master_key_epoch)`. Re-using the ticket for a different
//! chunk fails verification — the keymanager only releases the DEK when
//! HMAC + tenant-cert-SAN + expiry are all satisfied.
//!
//! `BatchFetchDek` (gate-1 F-H2) bundles up to 1024 tickets in one
//! request. Each is independently verified; partial failures are
//! reported per-ticket in the response.

use aws_lc_rs::hmac;
use kiseki_common::ids::{ChunkId, CompositionId, NamespaceId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use serde::{Deserialize, Serialize};

use super::signing_keys::SigningKeys;

const SCHEMA_VERSION: u8 = 1;
const HMAC_TAG_LEN: usize = 32;

/// Inner payload signed by the gateway and verified by the keymanager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DekFetchTicketInner {
    /// Wire-format version.
    pub schema_version: u8,
    /// Tenant the chunk belongs to (cross-checked against cert SAN).
    pub tenant_id: OrgId,
    /// Namespace.
    pub namespace_id: NamespaceId,
    /// Composition (file/object).
    pub composition_id: CompositionId,
    /// Specific chunk this ticket is good for.
    pub chunk_id: ChunkId,
    /// `crypto_boundary` at Read time — protects against a `TrustedCompute`
    /// → `ServerOnly` flip emitting a stale DEK.
    pub crypto_boundary_at_read: u8,
    /// Expiry millis since epoch.
    pub expires_at_millis_since_epoch: u64,
    /// Master-key epoch the ticket was minted under.
    pub master_key_epoch: KeyEpoch,
}

/// Errors raised by DEK-fetch-ticket serialization / verification.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum DekFetchTicketError {
    #[error("ticket too short")]
    TooShort,
    #[error("schema version {got} not supported (expected {expected})")]
    UnsupportedSchema { got: u8, expected: u8 },
    #[error("postcard decode: {0}")]
    Decode(String),
    #[error("postcard encode: {0}")]
    Encode(String),
    #[error("HMAC verification failed")]
    HmacInvalid,
    #[error("master-key epoch {0} unknown or out of grace")]
    KeyEpochUnknown(u64),
    #[error("ticket expired at {expired_at_ms} (now={now_ms})")]
    Expired { expired_at_ms: u64, now_ms: u64 },
    #[error("tenant mismatch")]
    TenantMismatch,
}

/// Serialize + sign a DEK-fetch ticket. Returns the on-the-wire bytes
/// the gateway hands to the client (and the client passes back to the
/// keymanager in `FetchDek` / `BatchFetchDek`).
pub fn serialize_signed(
    signing_keys: &SigningKeys,
    inner: &DekFetchTicketInner,
) -> Result<Vec<u8>, DekFetchTicketError> {
    let body =
        postcard::to_allocvec(inner).map_err(|e| DekFetchTicketError::Encode(e.to_string()))?;
    let key_bytes = signing_keys
        .dek_fetch_ticket_key(inner.master_key_epoch)
        .ok_or(DekFetchTicketError::KeyEpochUnknown(
            inner.master_key_epoch.0,
        ))?;
    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    let mut signed_input = Vec::with_capacity(1 + body.len());
    signed_input.push(SCHEMA_VERSION);
    signed_input.extend_from_slice(&body);
    let tag = hmac::sign(&mac_key, &signed_input);

    let mut out = Vec::with_capacity(1 + body.len() + HMAC_TAG_LEN);
    out.push(SCHEMA_VERSION);
    out.extend_from_slice(&body);
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

/// Verify HMAC + expiry + tenant binding. The keymanager calls this
/// when a tenant client presents a ticket with a `FetchDek` /
/// `BatchFetchDek` RPC. Returns the inner so the keymanager can
/// derive the DEK from `(master_key, chunk_id)`. `presenting_tenant`
/// is the tenant on the calling cert; `now_ms` is the current wall
/// clock used to enforce `expires_at_millis_since_epoch`.
pub fn verify_and_decode(
    signing_keys: &SigningKeys,
    ticket: &[u8],
    presenting_tenant: &OrgId,
    now_ms: u64,
) -> Result<DekFetchTicketInner, DekFetchTicketError> {
    if ticket.len() < 1 + HMAC_TAG_LEN {
        return Err(DekFetchTicketError::TooShort);
    }
    let schema_version = ticket[0];
    if schema_version != SCHEMA_VERSION {
        return Err(DekFetchTicketError::UnsupportedSchema {
            got: schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    let body_end = ticket.len() - HMAC_TAG_LEN;
    let body = &ticket[1..body_end];
    let tag = &ticket[body_end..];

    let inner: DekFetchTicketInner =
        postcard::from_bytes(body).map_err(|e| DekFetchTicketError::Decode(e.to_string()))?;

    let key_bytes = signing_keys
        .dek_fetch_ticket_key(inner.master_key_epoch)
        .ok_or(DekFetchTicketError::KeyEpochUnknown(
            inner.master_key_epoch.0,
        ))?;
    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    let mut signed_input = Vec::with_capacity(1 + body.len());
    signed_input.push(schema_version);
    signed_input.extend_from_slice(body);
    hmac::verify(&mac_key, &signed_input, tag).map_err(|_| DekFetchTicketError::HmacInvalid)?;

    if inner.tenant_id != *presenting_tenant {
        return Err(DekFetchTicketError::TenantMismatch);
    }
    if now_ms >= inner.expires_at_millis_since_epoch {
        return Err(DekFetchTicketError::Expired {
            expired_at_ms: inner.expires_at_millis_since_epoch,
            now_ms,
        });
    }
    Ok(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_crypto::keys::SystemMasterKey;

    fn signing(epoch: u64) -> SigningKeys {
        SigningKeys::new(&SystemMasterKey::new([0xAA; 32], KeyEpoch(epoch)), 60_000)
    }

    fn org(tag: u8) -> OrgId {
        OrgId(uuid::Uuid::from_bytes([tag; 16]))
    }

    fn sample(epoch: u64, tenant: OrgId, expires: u64) -> DekFetchTicketInner {
        DekFetchTicketInner {
            schema_version: SCHEMA_VERSION,
            tenant_id: tenant,
            namespace_id: NamespaceId(uuid::Uuid::nil()),
            composition_id: CompositionId(uuid::Uuid::nil()),
            chunk_id: ChunkId([0x55; 32]),
            crypto_boundary_at_read: 1,
            expires_at_millis_since_epoch: expires,
            master_key_epoch: KeyEpoch(epoch),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip() {
        let s = signing(1);
        let t = org(1);
        let bytes = serialize_signed(&s, &sample(1, t, 9_999_999_999)).unwrap();
        let decoded = verify_and_decode(&s, &bytes, &t, 1).unwrap();
        assert_eq!(decoded.tenant_id, t);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn expiry_rejected() {
        let s = signing(1);
        let t = org(1);
        let bytes = serialize_signed(&s, &sample(1, t, 1_000)).unwrap();
        let err = verify_and_decode(&s, &bytes, &t, 2_000).unwrap_err();
        assert!(matches!(err, DekFetchTicketError::Expired { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tenant_mismatch_rejected() {
        let s = signing(1);
        let alice = org(1);
        let bob = org(2);
        let bytes = serialize_signed(&s, &sample(1, alice, 9_999_999_999)).unwrap();
        let err = verify_and_decode(&s, &bytes, &bob, 1).unwrap_err();
        assert!(matches!(err, DekFetchTicketError::TenantMismatch));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flipped_byte_fails_hmac() {
        let s = signing(1);
        let t = org(1);
        let mut bytes = serialize_signed(&s, &sample(1, t, 9_999_999_999)).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let err = verify_and_decode(&s, &bytes, &t, 1).unwrap_err();
        assert!(matches!(err, DekFetchTicketError::HmacInvalid));
    }
}
