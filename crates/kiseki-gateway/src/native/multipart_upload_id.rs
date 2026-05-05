//! Self-describing multipart upload IDs (gate-1 round-2 N4).
//!
//! S3-style multipart uses an opaque `upload_id` string that the
//! server hands out on `InitMultipart` and the client echoes on every
//! subsequent `PutPart` / `Complete` / `Abort`. To prevent an attacker
//! from forging an ID for a tenant they don't own, the wire format is
//! HMAC-signed:
//!
//! ```text
//! [1 byte schema_version][postcard(MultipartUploadIdInner)][32 byte HMAC tag]
//! ```
//!
//! The string handed to the client is the base64url(no-pad) encoding of
//! that envelope so it survives HTTP path reflection if any pre-existing
//! S3 plumbing logs it. The native protocol serves the bytes verbatim
//! and has no path-encoding concern.
//!
//! HMAC keyed by `EpochKeys::multipart_upload_id`.

use aws_lc_rs::hmac;
use kiseki_common::ids::{NamespaceId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use serde::{Deserialize, Serialize};

use super::signing_keys::SigningKeys;

const SCHEMA_VERSION: u8 = 1;
const HMAC_TAG_LEN: usize = 32;

/// What the HMAC commits to. The native gateway stores its own
/// per-upload state keyed by `(tenant, namespace, internal_handle)` so
/// the wire ID need not encode the parts list — only the routing
/// triple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultipartUploadIdInner {
    /// Wire-format version.
    pub schema_version: u8,
    /// Tenant the upload belongs to (cross-checked against cert SAN).
    pub tenant_id: OrgId,
    /// Namespace.
    pub namespace_id: NamespaceId,
    /// Server-side opaque handle into the multipart-state table.
    /// 16 random bytes; the table is in-memory and ephemeral.
    pub internal_handle: [u8; 16],
    /// Issuance wall-clock millis (informational; the multipart
    /// state expiry lives in the in-memory table).
    pub issued_at_millis_since_epoch: u64,
    /// Master-key epoch the ID was minted under.
    pub master_key_epoch: KeyEpoch,
}

/// Errors raised by multipart-upload-id serialization / verification.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum MultipartUploadIdError {
    #[error("upload-id too short")]
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
    #[error("tenant mismatch")]
    TenantMismatch,
}

/// Serialize + sign a multipart upload-id envelope. Returns the
/// on-the-wire bytes the gateway emits in `InitMultipartResponse`.
pub fn serialize_signed(
    signing_keys: &SigningKeys,
    inner: &MultipartUploadIdInner,
) -> Result<Vec<u8>, MultipartUploadIdError> {
    let body = postcard::to_allocvec(inner)
        .map_err(|e| MultipartUploadIdError::Encode(e.to_string()))?;
    let key_bytes = signing_keys
        .multipart_upload_id_key(inner.master_key_epoch)
        .ok_or(MultipartUploadIdError::KeyEpochUnknown(
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

/// Verify HMAC + tenant binding, then return the decoded inner.
/// `presenting_tenant` is the canonical tenant on the calling cert
/// (a forged ID for tenant A presented over a tenant-B connection
/// is rejected).
pub fn verify_and_decode(
    signing_keys: &SigningKeys,
    upload_id: &[u8],
    presenting_tenant: &OrgId,
) -> Result<MultipartUploadIdInner, MultipartUploadIdError> {
    if upload_id.len() < 1 + HMAC_TAG_LEN {
        return Err(MultipartUploadIdError::TooShort);
    }
    let schema_version = upload_id[0];
    if schema_version != SCHEMA_VERSION {
        return Err(MultipartUploadIdError::UnsupportedSchema {
            got: schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    let body_end = upload_id.len() - HMAC_TAG_LEN;
    let body = &upload_id[1..body_end];
    let tag = &upload_id[body_end..];

    let inner: MultipartUploadIdInner =
        postcard::from_bytes(body).map_err(|e| MultipartUploadIdError::Decode(e.to_string()))?;

    let key_bytes = signing_keys
        .multipart_upload_id_key(inner.master_key_epoch)
        .ok_or(MultipartUploadIdError::KeyEpochUnknown(
            inner.master_key_epoch.0,
        ))?;
    let mac_key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    let mut signed_input = Vec::with_capacity(1 + body.len());
    signed_input.push(schema_version);
    signed_input.extend_from_slice(body);
    hmac::verify(&mac_key, &signed_input, tag).map_err(|_| MultipartUploadIdError::HmacInvalid)?;

    if inner.tenant_id != *presenting_tenant {
        return Err(MultipartUploadIdError::TenantMismatch);
    }
    Ok(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_crypto::keys::SystemMasterKey;

    fn signing() -> SigningKeys {
        SigningKeys::new(&SystemMasterKey::new([0xCC; 32], KeyEpoch(1)), 60_000)
    }

    fn org(tag: u8) -> OrgId {
        OrgId(uuid::Uuid::from_bytes([tag; 16]))
    }

    fn sample(tenant: OrgId) -> MultipartUploadIdInner {
        MultipartUploadIdInner {
            schema_version: SCHEMA_VERSION,
            tenant_id: tenant,
            namespace_id: NamespaceId(uuid::Uuid::nil()),
            internal_handle: [0xab; 16],
            issued_at_millis_since_epoch: 999,
            master_key_epoch: KeyEpoch(1),
        }
    }

    #[test]
    fn round_trip() {
        let s = signing();
        let t = org(1);
        let bytes = serialize_signed(&s, &sample(t)).unwrap();
        let decoded = verify_and_decode(&s, &bytes, &t).unwrap();
        assert_eq!(decoded.tenant_id, t);
    }

    #[test]
    fn tenant_mismatch_rejected() {
        let s = signing();
        let alice = org(1);
        let bob = org(2);
        let bytes = serialize_signed(&s, &sample(alice)).unwrap();
        let err = verify_and_decode(&s, &bytes, &bob).unwrap_err();
        assert!(matches!(err, MultipartUploadIdError::TenantMismatch));
    }

    #[test]
    fn flipped_byte_fails_hmac() {
        let s = signing();
        let t = org(1);
        let mut bytes = serialize_signed(&s, &sample(t)).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        let err = verify_and_decode(&s, &bytes, &t).unwrap_err();
        assert!(matches!(err, MultipartUploadIdError::HmacInvalid));
    }
}
