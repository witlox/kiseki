//! TCP-framed-postcard wire format for the native gateway data
//! service. ADR-042 §2.2.
//!
//! **Wire shape (V3 — post-2026-05-06 bulk-bytes-around-postcard
//! perf-fix)**:
//! - Request:  `[len u32 BE][ver u8][request_id u64 BE][verb_tag_len u8][verb_tag bytes][meta_len u32 BE][meta_bytes][bulk_bytes]`
//! - Response: `[len u32 BE][ver u8][status u8][request_id u64 BE][meta_len u32 BE][meta_bytes][bulk_bytes]`
//! - One TCP connection per (client, node); requests multiplex via
//!   `request_id`.
//!
//! `meta_bytes` is the postcard-encoded "metadata" struct — i.e.,
//! the verb's typed body MINUS its single bulk `Vec<u8>` field
//! (when present). `bulk_bytes` is the raw bytes of that bulk field
//! shipped DIRECTLY on the wire — no postcard framing — so a
//! 64 KiB GET response avoids the `postcard::serialize_bytes`
//! memcopy and the matching client-side decode copy. For verbs
//! with no bulk field, `bulk_bytes` is empty (length 0); the verb
//! body is fully in `meta_bytes`.
//!
//! Wire-format history:
//! - V1 (pre-2026-05-06): outer `RpcEnvelope` / `ResponseEnvelope`
//!   wrapping payload bytes in a postcard tuple — one full body
//!   memcopy per call on each side just from the wrap.
//! - V2 (mid-day 2026-05-06): hoisted `request_id` / `verb_tag` /
//!   `status` into fixed-width frame-header fields; verb body went
//!   straight on the wire without an outer envelope. Closed the V1
//!   double-memcopy.
//! - V3 (this version): split bulk Vec<u8> fields out of the
//!   postcard payload. Eliminates the per-call postcard
//!   encode/decode of the bulk bytes themselves on object verbs
//!   (put_object, get_object, write, read).
//!
//! Pre-1.0 wire-format break per ADR-042 §13.2: peers running V1/V2
//! see V3 frames as `UnsupportedVersion` and reject; operators wipe
//! + redeploy. No rolling upgrade across this bump.

// (No serde derive — V3 wire types are encoded by hand; the verb
// metadata structs are postcard-encoded by callers and ride as
// `meta_bytes` slices.)

/// Active wire-format version: V3 (split-bulk). Pre-1.0 — no V1/V2
/// back-compat. Peers running older versions hit `UnsupportedVersion`
/// and reject; operators wipe + redeploy.
pub const NATIVE_TCP_FRAMED_VERSION_V3: u8 = 3;

/// Maximum body size for a single TCP-framed message (request OR
/// response). Matches the per-stream cap (§1.5) at 64 MiB plus
/// headroom for the envelope overhead. Frames whose declared length
/// exceeds this are rejected before any allocation.
pub const NATIVE_TCP_FRAMED_MAX_BODY: usize = 80 * 1024 * 1024;

/// Reserved version-byte values that overlap with JSON document
/// starts (`[`, `{`, `"`). Permanently unassignable so a peer
/// pointed at an HTTP/JSON listener by mistake fails loudly with
/// `UnsupportedVersion` rather than mis-decoding into a postcard
/// envelope. Mirrors ADR-041 §"Reserved version-byte values".
pub const RESERVED_VERSION_BYTES: [u8; 3] = [0x5B, 0x7B, 0x22];

/// Server-side dispatch outcome for one TCP-framed call. ADR-042
/// §2.2 uses the same byte-status approach as the gRPC binding's
/// `tonic::Code` mapping per §1.4 error taxonomy; the bytes here are
/// fixed and explicitly numbered so the wire is self-describing.
///
/// `0x10`–`0x1F` matches the table in §1.4 exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WireStatus {
    /// Request succeeded; `payload_bytes` carries the postcard-encoded
    /// response body.
    Ok = 0x00,
    /// Frame failed parse before reaching the handler — wire-level
    /// error, not a `NativeError` variant. The caller should treat
    /// this as a connection-level fault.
    ProtocolError = 0x01,
    /// Verb identifier not known to this server (peer/server
    /// version skew).
    UnknownVerb = 0x02,
    /// `NativeError::Unauthenticated`.
    Unauthenticated = 0x10,
    /// `NativeError::PermissionDenied`.
    PermissionDenied = 0x11,
    /// `NativeError::InvalidArgument`.
    InvalidArgument = 0x12,
    /// `NativeError::NotFound`.
    NotFound = 0x13,
    /// `NativeError::AlreadyExists`.
    AlreadyExists = 0x14,
    /// `NativeError::PreconditionFailed`.
    PreconditionFailed = 0x15,
    /// `NativeError::OutOfRange`.
    OutOfRange = 0x16,
    /// `NativeError::ResourceExhausted`.
    ResourceExhausted = 0x17,
    /// `NativeError::Aborted`.
    Aborted = 0x18,
    /// `NativeError::Unavailable`.
    Unavailable = 0x19,
    /// `NativeError::NotLeader`. Body carries the leader node id (or
    /// nothing) per §1.4.
    NotLeader = 0x1A,
    /// `NativeError::Internal` (server-side bug).
    Internal = 0x1F,
}

impl WireStatus {
    /// Decode a status byte. `None` for unknown values — caller
    /// should treat as a protocol-level error and tear down the
    /// connection.
    #[must_use]
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Ok,
            0x01 => Self::ProtocolError,
            0x02 => Self::UnknownVerb,
            0x10 => Self::Unauthenticated,
            0x11 => Self::PermissionDenied,
            0x12 => Self::InvalidArgument,
            0x13 => Self::NotFound,
            0x14 => Self::AlreadyExists,
            0x15 => Self::PreconditionFailed,
            0x16 => Self::OutOfRange,
            0x17 => Self::ResourceExhausted,
            0x18 => Self::Aborted,
            0x19 => Self::Unavailable,
            0x1A => Self::NotLeader,
            0x1F => Self::Internal,
            _ => return None,
        })
    }
}

/// Errors raised by frame encode/decode. Not the same as
/// [`crate::native_contract::NativeError`] — these are wire-level
/// conditions that fire BEFORE a handler ever sees the request.
#[derive(Debug, thiserror::Error)]
pub enum WireDecodeError {
    /// Buffer ended before a complete frame could be read.
    #[error("incomplete frame: have {have} bytes, need {need}")]
    Incomplete {
        /// Bytes available.
        have: usize,
        /// Bytes required to advance.
        need: usize,
    },
    /// Declared body length exceeds [`NATIVE_TCP_FRAMED_MAX_BODY`].
    /// Pre-allocation guard against malicious peers.
    #[error("frame body length {len} exceeds cap {cap}")]
    Oversize {
        /// Declared length from the wire.
        len: usize,
        /// Cap (`NATIVE_TCP_FRAMED_MAX_BODY`).
        cap: usize,
    },
    /// Version byte didn't match the supported value(s) — peer
    /// running incompatible build.
    #[error("unsupported wire version {version}; this build supports {supported}")]
    UnsupportedVersion {
        /// Version byte from the wire.
        version: u8,
        /// Version this build accepts.
        supported: u8,
    },
    /// postcard decode failed on the envelope or response payload.
    #[error("postcard decode failed: {0}")]
    Postcard(String),
}

/// Errors raised by frame encode. Encode is mostly infallible — the
/// only failure mode is the body exceeding the cap once postcard has
/// produced the bytes.
#[derive(Debug, thiserror::Error)]
pub enum WireEncodeError {
    /// postcard encode failed (allocation or unrepresentable value).
    #[error("postcard encode failed: {0}")]
    Postcard(String),
    /// Encoded body exceeds [`NATIVE_TCP_FRAMED_MAX_BODY`]. Caller
    /// must split (multipart for object writes; non-streaming verbs
    /// have no split path and surface this as `OutOfRange` to the
    /// caller).
    #[error("frame body length {len} exceeds cap {cap}")]
    Oversize {
        /// Encoded length.
        len: usize,
        /// Cap.
        cap: usize,
    },
}

/// Decoded request frame view (V3). Holds borrowed slices into the
/// caller's buffer — no copies. `meta` is what dispatch passes to
/// `postcard::from_bytes` into the verb's typed request struct
/// (with bulk fields empty); `bulk` is the raw bulk-field bytes the
/// caller attaches into the typed struct after decoding.
#[derive(Debug)]
pub struct RequestFrameView<'a> {
    /// Echoed by the response so the client can demultiplex.
    pub request_id: u64,
    /// Verb identifier. UTF-8; ≤ 255 bytes.
    pub verb_tag: &'a str,
    /// Postcard-encoded metadata — the typed request struct minus
    /// any bulk Vec<u8> field.
    pub meta: &'a [u8],
    /// Raw bulk bytes (empty for verbs with no bulk field).
    pub bulk: &'a [u8],
}

/// Decoded response frame view (V3). Borrowed slices.
#[derive(Debug)]
pub struct ResponseFrameView<'a> {
    /// Status byte mapped to ADR-042 §1.4.
    pub status: WireStatus,
    /// Echoed from the request so the client correlates.
    pub request_id: u64,
    /// On `WireStatus::Ok`: postcard-encoded metadata struct (verb
    /// response minus its bulk Vec<u8>). On error statuses: UTF-8
    /// reason string (the meta_len carries the full message; bulk
    /// is empty on error).
    pub meta: &'a [u8],
    /// Raw bulk-field bytes, or empty for non-bulk verbs / errors.
    pub bulk: &'a [u8],
}

/// Maximum verb-tag length on the wire. `u8::MAX` (255) — every
/// kiseki verb name is ≤ 24 chars, so this is generous headroom.
const MAX_VERB_TAG_LEN: usize = u8::MAX as usize;

/// V3 response body header: version u8 + status u8 + request_id u64
/// + meta_len u32 = 14. Bulk follows after `meta_len` bytes of
/// `meta_bytes`; total frame body = 14 + meta_len + bulk_len.
pub const RESPONSE_HEADER_LEN: usize = 1 + 1 + 8 + 4;

/// V3 request body fixed header: version u8 + request_id u64 +
/// verb_tag_len u8 = 10. Followed by `verb_tag bytes` then
/// `meta_len u32 BE` then `meta_bytes` then `bulk_bytes`. Total
/// frame body = 10 + verb_tag_len + 4 + meta_len + bulk_len.
pub const REQUEST_HEADER_FIXED_LEN: usize = 1 + 8 + 1;

/// Build a request frame *header* — everything up to (and
/// including) the meta_len prefix. Caller follows with separate
/// writes of `meta_bytes` and `bulk_bytes`; the wire layout is
/// `[len][ver][rid][verb_len][verb][meta_len][meta][bulk]`.
///
/// Vectored I/O on the call site makes the three pieces a single
/// `writev` syscall on Linux — no Nagle, no second memcopy.
///
/// # Errors
/// `Oversize` if the resulting frame exceeds
/// [`NATIVE_TCP_FRAMED_MAX_BODY`]. `Postcard` if `verb_tag` is
/// longer than [`MAX_VERB_TAG_LEN`] (255 bytes).
#[allow(clippy::missing_errors_doc)]
pub fn build_request_header(
    request_id: u64,
    verb_tag: &str,
    meta_len: usize,
    bulk_len: usize,
) -> Result<Vec<u8>, WireEncodeError> {
    let verb_bytes = verb_tag.as_bytes();
    if verb_bytes.len() > MAX_VERB_TAG_LEN {
        return Err(WireEncodeError::Postcard(format!(
            "verb_tag length {} exceeds wire cap {MAX_VERB_TAG_LEN}",
            verb_bytes.len()
        )));
    }
    let total_body_len = REQUEST_HEADER_FIXED_LEN + verb_bytes.len() + 4 + meta_len + bulk_len;
    if total_body_len > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireEncodeError::Oversize {
            len: total_body_len,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    let mut out = Vec::with_capacity(4 + REQUEST_HEADER_FIXED_LEN + verb_bytes.len() + 4);
    out.extend_from_slice(
        &u32::try_from(total_body_len)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.push(NATIVE_TCP_FRAMED_VERSION_V3);
    out.extend_from_slice(&request_id.to_be_bytes());
    #[allow(clippy::cast_possible_truncation)]
    out.push(verb_bytes.len() as u8);
    out.extend_from_slice(verb_bytes);
    out.extend_from_slice(&u32::try_from(meta_len).unwrap_or(u32::MAX).to_be_bytes());
    Ok(out)
}

/// Encode a complete request frame in a single allocation —
/// convenience wrapper for tests and small-payload callers.
/// Hot-path callers prefer [`build_request_header`] + vectored I/O
/// of `[header, meta, bulk]` to skip the body memcopy.
///
/// # Errors
/// `Oversize`, `Postcard` (verb_tag too long).
#[allow(clippy::missing_errors_doc)]
pub fn encode_request_frame(
    request_id: u64,
    verb_tag: &str,
    meta: &[u8],
    bulk: &[u8],
) -> Result<Vec<u8>, WireEncodeError> {
    let mut out = build_request_header(request_id, verb_tag, meta.len(), bulk.len())?;
    out.extend_from_slice(meta);
    out.extend_from_slice(bulk);
    Ok(out)
}

/// Decode a V3 request frame from a length-prefix-stripped buffer.
/// Returns borrowed slices for `meta` and `bulk` — no copies.
///
/// Input layout:
/// `[ver u8][request_id u64 BE][verb_tag_len u8][verb_tag bytes][meta_len u32 BE][meta_bytes][bulk_bytes]`.
///
/// # Errors
/// `Incomplete` / `UnsupportedVersion` / `Postcard` (invalid UTF-8
/// in verb_tag, or meta_len overruns the body).
#[allow(clippy::missing_errors_doc)]
pub fn decode_request_frame(body: &[u8]) -> Result<RequestFrameView<'_>, WireDecodeError> {
    if body.len() < REQUEST_HEADER_FIXED_LEN {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: REQUEST_HEADER_FIXED_LEN,
        });
    }
    let version = body[0];
    if version != NATIVE_TCP_FRAMED_VERSION_V3 {
        return Err(WireDecodeError::UnsupportedVersion {
            version,
            supported: NATIVE_TCP_FRAMED_VERSION_V3,
        });
    }
    let request_id = u64::from_be_bytes(body[1..9].try_into().unwrap());
    let verb_tag_len = body[9] as usize;
    let verb_tag_end = REQUEST_HEADER_FIXED_LEN + verb_tag_len;
    if body.len() < verb_tag_end + 4 {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: verb_tag_end + 4,
        });
    }
    let verb_tag_bytes = &body[REQUEST_HEADER_FIXED_LEN..verb_tag_end];
    let verb_tag = std::str::from_utf8(verb_tag_bytes)
        .map_err(|e| WireDecodeError::Postcard(format!("verb_tag not utf-8: {e}")))?;
    let meta_len =
        u32::from_be_bytes(body[verb_tag_end..verb_tag_end + 4].try_into().unwrap()) as usize;
    let meta_start = verb_tag_end + 4;
    let meta_end = meta_start + meta_len;
    if body.len() < meta_end {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: meta_end,
        });
    }
    Ok(RequestFrameView {
        request_id,
        verb_tag,
        meta: &body[meta_start..meta_end],
        bulk: &body[meta_end..],
    })
}

/// Build a V3 response frame *header* — length prefix + version +
/// status + request_id + meta_len. Fixed 18 bytes. Caller follows
/// with `meta_bytes` and `bulk_bytes`; vectored I/O sends all three
/// in one syscall.
///
/// Header layout: `[len u32 BE][ver u8][status u8][request_id u64 BE][meta_len u32 BE]`.
///
/// # Errors
/// `Oversize` if the total frame exceeds
/// [`NATIVE_TCP_FRAMED_MAX_BODY`].
#[allow(clippy::missing_errors_doc)]
pub fn build_response_header(
    status: WireStatus,
    request_id: u64,
    meta_len: usize,
    bulk_len: usize,
) -> Result<[u8; 4 + RESPONSE_HEADER_LEN], WireEncodeError> {
    let total_body_len = RESPONSE_HEADER_LEN + meta_len + bulk_len;
    if total_body_len > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireEncodeError::Oversize {
            len: total_body_len,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    let mut header = [0u8; 4 + RESPONSE_HEADER_LEN];
    header[0..4].copy_from_slice(
        &u32::try_from(total_body_len)
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    header[4] = NATIVE_TCP_FRAMED_VERSION_V3;
    header[5] = status as u8;
    header[6..14].copy_from_slice(&request_id.to_be_bytes());
    header[14..18].copy_from_slice(&u32::try_from(meta_len).unwrap_or(u32::MAX).to_be_bytes());
    Ok(header)
}

/// Encode a complete response frame in a single allocation —
/// convenience wrapper for tests + small-payload paths. Hot-path
/// callers use vectored I/O on `[header, meta, bulk]` to skip the
/// body memcopy.
///
/// # Errors
/// `Oversize` if the body exceeds [`NATIVE_TCP_FRAMED_MAX_BODY`].
#[allow(clippy::missing_errors_doc)]
pub fn encode_response_frame(
    status: WireStatus,
    request_id: u64,
    meta: &[u8],
    bulk: &[u8],
) -> Result<Vec<u8>, WireEncodeError> {
    let header = build_response_header(status, request_id, meta.len(), bulk.len())?;
    let mut out = Vec::with_capacity(header.len() + meta.len() + bulk.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(meta);
    out.extend_from_slice(bulk);
    Ok(out)
}

/// Decode a V3 response frame body (length-prefix-stripped).
/// Returns borrowed slices for `meta` and `bulk`.
///
/// Input layout:
/// `[ver u8][status u8][request_id u64 BE][meta_len u32 BE][meta_bytes][bulk_bytes]`.
///
/// # Errors
/// `Incomplete` / `UnsupportedVersion` / `Postcard` (unknown
/// status byte, or meta_len overruns body).
#[allow(clippy::missing_errors_doc)]
pub fn decode_response_frame(body: &[u8]) -> Result<ResponseFrameView<'_>, WireDecodeError> {
    if body.len() < RESPONSE_HEADER_LEN {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: RESPONSE_HEADER_LEN,
        });
    }
    let version = body[0];
    if version != NATIVE_TCP_FRAMED_VERSION_V3 {
        return Err(WireDecodeError::UnsupportedVersion {
            version,
            supported: NATIVE_TCP_FRAMED_VERSION_V3,
        });
    }
    let status = WireStatus::from_u8(body[1]).ok_or_else(|| {
        WireDecodeError::Postcard(format!("unknown status byte 0x{:02x}", body[1]))
    })?;
    let request_id = u64::from_be_bytes(body[2..10].try_into().unwrap());
    let meta_len = u32::from_be_bytes(body[10..14].try_into().unwrap()) as usize;
    let meta_start = RESPONSE_HEADER_LEN;
    let meta_end = meta_start + meta_len;
    if body.len() < meta_end {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: meta_end,
        });
    }
    Ok(ResponseFrameView {
        status,
        request_id,
        meta: &body[meta_start..meta_end],
        bulk: &body[meta_end..],
    })
}

/// Validate a length prefix read from the wire. Caller has already
/// read the four-byte BE length; this enforces the cap and returns
/// the validated body length so the caller knows how many bytes to
/// pull next. Splits the cap check from the read loop so the loop
/// stays simple.
#[allow(clippy::missing_errors_doc)]
pub fn validate_frame_length(length_be: u32) -> Result<usize, WireDecodeError> {
    let len = length_be as usize;
    if len > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireDecodeError::Oversize {
            len,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_pinned() {
        // V3 (split-bulk) is the active wire version. Pin so a
        // refactor that bumps it forces a conscious decision +
        // operator wipe-and-redeploy notice.
        assert_eq!(NATIVE_TCP_FRAMED_VERSION_V3, 3);
    }

    #[test]
    fn reserved_version_bytes_match_json_starts() {
        // `[` `{` `"` — JSON document starts. Pin so a refactor
        // doesn't inadvertently assign a version byte that overlaps.
        assert_eq!(RESERVED_VERSION_BYTES, [0x5B, 0x7B, 0x22]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V3, RESERVED_VERSION_BYTES[0]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V3, RESERVED_VERSION_BYTES[1]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V3, RESERVED_VERSION_BYTES[2]);
    }

    #[test]
    fn request_frame_roundtrip_meta_and_bulk() {
        let meta = b"meta-postcard-bytes";
        let bulk = b"raw-bulk-bytes";
        let framed = encode_request_frame(42, "get_object", meta, bulk).expect("encode");
        assert!(framed.len() > 4);
        let length = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        let frame_body = &framed[4..];
        assert_eq!(frame_body.len(), length);
        let view = decode_request_frame(frame_body).expect("decode");
        assert_eq!(view.request_id, 42);
        assert_eq!(view.verb_tag, "get_object");
        assert_eq!(view.meta, meta);
        assert_eq!(view.bulk, bulk);
    }

    #[test]
    fn request_frame_with_empty_bulk_round_trips() {
        // Non-bulk verbs send an empty bulk slice.
        let meta = b"x";
        let framed = encode_request_frame(1, "get_topology", meta, &[]).expect("encode");
        let view = decode_request_frame(&framed[4..]).expect("decode");
        assert_eq!(view.meta, meta);
        assert!(view.bulk.is_empty());
    }

    #[test]
    fn request_frame_starts_with_supported_version_byte() {
        let framed = encode_request_frame(0, "x", &[], &[]).expect("encode");
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V3);
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        let mut body = vec![99u8];
        body.extend_from_slice(&[0; 9]);
        let err = decode_request_frame(&body).expect_err("must reject");
        match err {
            WireDecodeError::UnsupportedVersion { version, supported } => {
                assert_eq!(version, 99);
                assert_eq!(supported, NATIVE_TCP_FRAMED_VERSION_V3);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn decode_empty_body_is_incomplete() {
        let err = decode_request_frame(&[]).expect_err("must reject");
        match err {
            WireDecodeError::Incomplete { have, need } => {
                assert_eq!(have, 0);
                assert_eq!(need, REQUEST_HEADER_FIXED_LEN);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn decode_request_with_invalid_utf8_verb_tag_rejected() {
        let mut frame_body = vec![NATIVE_TCP_FRAMED_VERSION_V3];
        frame_body.extend_from_slice(&0u64.to_be_bytes()); // request_id
        frame_body.push(2); // verb_tag_len
        frame_body.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
        frame_body.extend_from_slice(&0u32.to_be_bytes()); // meta_len
        let err = decode_request_frame(&frame_body).expect_err("must reject");
        assert!(matches!(err, WireDecodeError::Postcard(_)));
    }

    #[test]
    fn validate_frame_length_rejects_oversize() {
        let cap = NATIVE_TCP_FRAMED_MAX_BODY;
        let huge = u32::try_from(cap + 1).unwrap_or(u32::MAX);
        let err = validate_frame_length(huge).expect_err("must reject");
        match err {
            WireDecodeError::Oversize { len, cap: c } => {
                assert!(len > c);
                assert_eq!(c, NATIVE_TCP_FRAMED_MAX_BODY);
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn validate_frame_length_accepts_at_cap() {
        let cap = u32::try_from(NATIVE_TCP_FRAMED_MAX_BODY).unwrap_or(u32::MAX);
        let len = validate_frame_length(cap).expect("at-cap accepted");
        assert_eq!(len, NATIVE_TCP_FRAMED_MAX_BODY);
    }

    #[test]
    fn response_frame_roundtrip_ok() {
        let meta = b"meta-bytes";
        let bulk = b"raw-bulk-bytes";
        let framed = encode_response_frame(WireStatus::Ok, 99, meta, bulk).expect("encode");
        let body = &framed[4..];
        let view = decode_response_frame(body).expect("decode");
        assert_eq!(view.status, WireStatus::Ok);
        assert_eq!(view.request_id, 99);
        assert_eq!(view.meta, meta);
        assert_eq!(view.bulk, bulk);
    }

    #[test]
    fn response_frame_with_empty_bulk_round_trips() {
        let meta = b"only-meta";
        let framed = encode_response_frame(WireStatus::Ok, 1, meta, &[]).expect("encode");
        let view = decode_response_frame(&framed[4..]).expect("decode");
        assert_eq!(view.meta, meta);
        assert!(view.bulk.is_empty());
    }

    #[test]
    fn response_frame_roundtrip_error_status() {
        let payload = b"reason: san_payload_tenant_mismatch";
        // Error reason rides in `meta` (postcard-decoded as a String
        // by the caller); bulk is empty on error frames.
        let framed =
            encode_response_frame(WireStatus::PermissionDenied, 7, payload, &[]).expect("encode");
        let body = &framed[4..];
        let view = decode_response_frame(body).expect("decode");
        assert_eq!(view.status, WireStatus::PermissionDenied);
        assert_eq!(view.request_id, 7);
        assert_eq!(view.meta, payload);
        assert!(view.bulk.is_empty());
    }

    #[test]
    fn response_frame_short_body_is_incomplete() {
        let body = vec![NATIVE_TCP_FRAMED_VERSION_V3];
        let err = decode_response_frame(&body).expect_err("must reject");
        assert!(matches!(err, WireDecodeError::Incomplete { .. }));
    }

    #[test]
    fn response_frame_unknown_status_byte_rejected() {
        let mut body = vec![NATIVE_TCP_FRAMED_VERSION_V3, 0x77];
        body.extend_from_slice(&[0; 8]); // request_id padding
        body.extend_from_slice(&0u32.to_be_bytes()); // meta_len padding
        let err = decode_response_frame(&body).expect_err("must reject");
        assert!(matches!(err, WireDecodeError::Postcard(_)));
    }

    #[test]
    fn wire_status_byte_values_pinned_to_native_error_table() {
        // ADR-042 §1.4 → §2.2: Unauthenticated 0x10, PermissionDenied
        // 0x11, ..., NotLeader 0x1A, Internal 0x1F. Pin so a refactor
        // that renumbers also breaks this test.
        assert_eq!(WireStatus::Ok as u8, 0x00);
        assert_eq!(WireStatus::ProtocolError as u8, 0x01);
        assert_eq!(WireStatus::UnknownVerb as u8, 0x02);
        assert_eq!(WireStatus::Unauthenticated as u8, 0x10);
        assert_eq!(WireStatus::PermissionDenied as u8, 0x11);
        assert_eq!(WireStatus::InvalidArgument as u8, 0x12);
        assert_eq!(WireStatus::NotFound as u8, 0x13);
        assert_eq!(WireStatus::AlreadyExists as u8, 0x14);
        assert_eq!(WireStatus::PreconditionFailed as u8, 0x15);
        assert_eq!(WireStatus::OutOfRange as u8, 0x16);
        assert_eq!(WireStatus::ResourceExhausted as u8, 0x17);
        assert_eq!(WireStatus::Aborted as u8, 0x18);
        assert_eq!(WireStatus::Unavailable as u8, 0x19);
        assert_eq!(WireStatus::NotLeader as u8, 0x1A);
        assert_eq!(WireStatus::Internal as u8, 0x1F);
    }

    #[test]
    fn wire_status_from_u8_round_trips() {
        for status in [
            WireStatus::Ok,
            WireStatus::ProtocolError,
            WireStatus::UnknownVerb,
            WireStatus::Unauthenticated,
            WireStatus::PermissionDenied,
            WireStatus::InvalidArgument,
            WireStatus::NotFound,
            WireStatus::AlreadyExists,
            WireStatus::PreconditionFailed,
            WireStatus::OutOfRange,
            WireStatus::ResourceExhausted,
            WireStatus::Aborted,
            WireStatus::Unavailable,
            WireStatus::NotLeader,
            WireStatus::Internal,
        ] {
            let byte = status as u8;
            let decoded = WireStatus::from_u8(byte).expect("known");
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn wire_status_from_u8_rejects_unassigned_values() {
        // 0x03..=0x0F is a reserved gap between connection-level
        // statuses (0x00..=0x02) and NativeError-mapped statuses
        // (0x10..=0x1F). 0x1B..=0x1E is the unassigned tail. Pin a
        // few representatives.
        for byte in [0x03u8, 0x05, 0x0F, 0x1B, 0x1E, 0x20, 0xFF] {
            assert!(
                WireStatus::from_u8(byte).is_none(),
                "unassigned byte 0x{byte:02x} must decode to None",
            );
        }
    }

    #[test]
    fn encode_request_frame_oversize_rejected() {
        let body = vec![0u8; NATIVE_TCP_FRAMED_MAX_BODY];
        let err = encode_request_frame(0, "x", &[], &body).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Oversize { .. }));
    }

    #[test]
    fn encode_response_frame_oversize_rejected() {
        let payload = vec![0u8; NATIVE_TCP_FRAMED_MAX_BODY];
        let err = encode_response_frame(WireStatus::Ok, 0, &[], &payload).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Oversize { .. }));
    }

    #[test]
    fn encode_request_with_oversize_verb_tag_rejected() {
        let big_verb = "x".repeat(256);
        let err = encode_request_frame(0, &big_verb, &[], &[]).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Postcard(_)));
    }

    /// End-to-end V3 framing layout assertion — proves the wire
    /// bytes match the layout described in the module docs.
    #[test]
    fn request_frame_layout_matches_spec() {
        let meta = b"\xCA\xFE";
        let bulk = b"\xBA\xBE";
        let framed = encode_request_frame(7, "ping", meta, bulk).expect("encode");
        // [len u32 BE]
        let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(len, framed.len() - 4);
        // [ver u8]
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V3);
        // [request_id u64 BE]
        let request_id = u64::from_be_bytes(framed[5..13].try_into().unwrap());
        assert_eq!(request_id, 7);
        // [verb_tag_len u8]
        assert_eq!(framed[13], 4); // "ping" is 4 bytes
                                   // [verb_tag bytes]
        assert_eq!(&framed[14..18], b"ping");
        // [meta_len u32 BE]
        let meta_len = u32::from_be_bytes(framed[18..22].try_into().unwrap()) as usize;
        assert_eq!(meta_len, meta.len());
        // [meta_bytes]
        assert_eq!(&framed[22..22 + meta_len], meta);
        // [bulk_bytes]
        assert_eq!(&framed[22 + meta_len..], bulk);
    }

    /// V3 response frame layout pin.
    #[test]
    fn response_frame_layout_matches_spec() {
        let meta = b"\xDE\xAD";
        let bulk = b"\xBE\xEF";
        let framed = encode_response_frame(WireStatus::Ok, 13, meta, bulk).expect("encode");
        let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(len, framed.len() - 4);
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V3);
        assert_eq!(framed[5], WireStatus::Ok as u8);
        let request_id = u64::from_be_bytes(framed[6..14].try_into().unwrap());
        assert_eq!(request_id, 13);
        let meta_len = u32::from_be_bytes(framed[14..18].try_into().unwrap()) as usize;
        assert_eq!(meta_len, meta.len());
        assert_eq!(&framed[18..18 + meta_len], meta);
        assert_eq!(&framed[18 + meta_len..], bulk);
    }
}
