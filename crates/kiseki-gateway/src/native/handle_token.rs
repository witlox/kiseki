//! POSIX file handle tokens (ADR-042 §9, I-NG10).
//!
//! Opaque server-signed token issued by `Open`, presented on every
//! subsequent `Read`/`Write`/`Fsync`/`Close`. Wire format:
//!
//! ```text
//! [1 byte schema_version][postcard(HandleTokenInner)][32 byte HMAC tag]
//! ```
//!
//! HMAC keyed by `EpochKeys::handle_token` for `inner.master_key_epoch`
//! (gate-1 F-C1 — accepted across rotation grace window).
//!
//! The token is BOUND to the cert SAN that minted it (gate-1 F-H1).
//! `verify_and_decode` rejects with `SanMismatch` when the presented
//! connection's canonical SAN URI differs from `inner.cert_san_canonical`,
//! even if the HMAC tag is valid.

use aws_lc_rs::hmac;
use kiseki_common::ids::NamespaceId;
use kiseki_common::tenancy::KeyEpoch;
use serde::{Deserialize, Serialize};

use super::signing_keys::SigningKeys;

const SCHEMA_VERSION: u8 = 1;
const HMAC_TAG_LEN: usize = 32;

/// Inner payload — what the HMAC commits to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandleTokenInner {
    /// Wire-format version. Bumped when the postcard schema changes.
    pub schema_version: u8,
    /// Tenant namespace the handle is bound to.
    pub namespace_id: NamespaceId,
    /// POSIX inode number this handle refers to.
    pub inode: u64,
    /// Open mode (1 = READ, 2 = WRITE, 3 = `READ_WRITE` — mirrors
    /// `kiseki.v1.native.OpenMode`).
    pub open_mode: u8,
    /// `crypto_boundary` of the namespace at open time. `ServerOnly` = 0,
    /// `TrustedCompute` = 1. Used so a tenant flipping the boundary
    /// doesn't change semantics for already-open handles.
    pub crypto_boundary_at_open: u8,
    /// Canonical SAN URI of the cert that opened this handle (gate-1
    /// F-H1). Must byte-equal the connection's cert SAN at every use.
    pub cert_san_canonical: String,
    /// Master-key epoch the token was minted under. The HMAC verify
    /// path looks up the matching `EpochKeys::handle_token` and
    /// fails (`KeyEpochUnknown`) if it's out of grace.
    pub master_key_epoch: KeyEpoch,
    /// Issuance wall-clock millis. Used for soft TTL bookkeeping.
    pub issued_at_millis_since_epoch: u64,
    /// Random nonce so two handles for the same (inode, mode, cert,
    /// epoch) at the same millisecond don't compare equal — defense
    /// against linkability across clients sharing a mount.
    pub issuance_nonce: [u8; 16],
}

/// Errors raised by handle-token serialization / verification. Each
/// variant maps 1:1 to a `tonic::Status` code at the proto-handler
/// boundary (`InvalidArgument` for decode/schema, `Unauthenticated`
/// for HMAC / SAN / epoch failures).
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum HandleTokenError {
    #[error("token too short")]
    TooShort,
    #[error("schema version {got} not supported (expected {expected})")]
    UnsupportedSchema { got: u8, expected: u8 },
    #[error("postcard decode: {0}")]
    Decode(String),
    #[error("HMAC verification failed")]
    HmacInvalid,
    #[error("master-key epoch {0} unknown or out of grace")]
    KeyEpochUnknown(u64),
    #[error("cert SAN mismatch (token bound to different cert)")]
    SanMismatch,
    #[error("postcard encode: {0}")]
    Encode(String),
}

/// Serialize + sign a handle-token inner body. Caller is responsible
/// for ensuring `inner.master_key_epoch == signing_keys.current_epoch()`
/// at mint time. Returns the on-the-wire token bytes.
pub fn serialize_signed(
    signing_keys: &SigningKeys,
    inner: &HandleTokenInner,
) -> Result<Vec<u8>, HandleTokenError> {
    let body = postcard::to_allocvec(inner).map_err(|e| HandleTokenError::Encode(e.to_string()))?;
    let key_bytes = signing_keys
        .handle_token_key(inner.master_key_epoch)
        .ok_or(HandleTokenError::KeyEpochUnknown(inner.master_key_epoch.0))?;

    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    // HMAC over [schema_version || body] so the framing byte is bound.
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

/// Verify HMAC + cert SAN binding, then return the decoded inner.
/// `presented_cert_san` is the canonicalized SAN URI of the
/// connection presenting the token (compared byte-equal to the value
/// embedded in the token body — gate-1 F-H1).
pub fn verify_and_decode(
    signing_keys: &SigningKeys,
    token: &[u8],
    presented_cert_san: &str,
) -> Result<HandleTokenInner, HandleTokenError> {
    if token.len() < 1 + HMAC_TAG_LEN {
        return Err(HandleTokenError::TooShort);
    }
    let schema_version = token[0];
    if schema_version != SCHEMA_VERSION {
        return Err(HandleTokenError::UnsupportedSchema {
            got: schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    let body_end = token.len() - HMAC_TAG_LEN;
    let body = &token[1..body_end];
    let tag = &token[body_end..];

    let inner: HandleTokenInner =
        postcard::from_bytes(body).map_err(|e| HandleTokenError::Decode(e.to_string()))?;

    let key_bytes = signing_keys
        .handle_token_key(inner.master_key_epoch)
        .ok_or(HandleTokenError::KeyEpochUnknown(inner.master_key_epoch.0))?;
    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    let mut signed_input = Vec::with_capacity(1 + body.len());
    signed_input.push(schema_version);
    signed_input.extend_from_slice(body);
    hmac::verify(&mac_key, &signed_input, tag).map_err(|_| HandleTokenError::HmacInvalid)?;

    if inner.cert_san_canonical != presented_cert_san {
        return Err(HandleTokenError::SanMismatch);
    }
    Ok(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_crypto::keys::SystemMasterKey;

    fn keys(epoch: u64) -> SigningKeys {
        SigningKeys::new(&SystemMasterKey::new([0x33; 32], KeyEpoch(epoch)), 60_000)
    }

    fn sample_inner(epoch: u64, san: &str) -> HandleTokenInner {
        HandleTokenInner {
            schema_version: SCHEMA_VERSION,
            namespace_id: NamespaceId(uuid::Uuid::nil()),
            inode: 123,
            open_mode: 3,
            crypto_boundary_at_open: 0,
            cert_san_canonical: san.to_string(),
            master_key_epoch: KeyEpoch(epoch),
            issued_at_millis_since_epoch: 42,
            issuance_nonce: [0xab; 16],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_succeeds() {
        let s = keys(1);
        let san = "spiffe://kiseki/tenant/org-x";
        let bytes = serialize_signed(&s, &sample_inner(1, san)).unwrap();
        let decoded = verify_and_decode(&s, &bytes, san).unwrap();
        assert_eq!(decoded.inode, 123);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flipped_byte_fails_hmac() {
        let s = keys(1);
        let san = "spiffe://kiseki/tenant/org-x";
        let mut bytes = serialize_signed(&s, &sample_inner(1, san)).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let err = verify_and_decode(&s, &bytes, san).unwrap_err();
        assert!(matches!(err, HandleTokenError::HmacInvalid));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn san_mismatch_rejected_even_with_valid_hmac() {
        let s = keys(1);
        let san_a = "spiffe://kiseki/tenant/org-a";
        let san_b = "spiffe://kiseki/tenant/org-b";
        let bytes = serialize_signed(&s, &sample_inner(1, san_a)).unwrap();
        let err = verify_and_decode(&s, &bytes, san_b).unwrap_err();
        assert!(matches!(err, HandleTokenError::SanMismatch));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_epoch_rejected() {
        let s = keys(5);
        let san = "spiffe://kiseki/tenant/org-x";
        // Mint with epoch 99 which is not in the store.
        let inner = sample_inner(99, san);
        let err = serialize_signed(&s, &inner).unwrap_err();
        assert!(matches!(err, HandleTokenError::KeyEpochUnknown(99)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rotation_grace_keeps_old_tokens_valid() {
        let store = keys(1);
        let san = "spiffe://kiseki/tenant/org-x";
        let bytes = serialize_signed(&store, &sample_inner(1, san)).unwrap();
        // Rotate to epoch 2; old tokens still verify during grace.
        let new = SystemMasterKey::new([0x44; 32], KeyEpoch(2));
        store.rotate(&new, 1_000);
        let decoded = verify_and_decode(&store, &bytes, san).unwrap();
        assert_eq!(decoded.master_key_epoch, KeyEpoch(1));
        // After grace, the same token fails.
        store.prune_expired(2_000_000);
        let err = verify_and_decode(&store, &bytes, san).unwrap_err();
        assert!(matches!(err, HandleTokenError::KeyEpochUnknown(1)));
    }
}
