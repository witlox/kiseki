//! TCP-framed-postcard wire format for the native gateway data
//! service. ADR-042 §2.2.
//!
//! **Wire shape (V2 — post-2026-05-06 perf-fix)**:
//! - Request:  `[len u32 BE][ver u8][request_id u64 BE][verb_tag_len u8][verb_tag bytes][postcard(verb_request_body)...]`
//! - Response: `[len u32 BE][ver u8][status u8][request_id u64 BE][postcard(verb_response_body)...]`
//! - One TCP connection per (client, node); requests multiplex via
//!   `request_id`.
//!
//! V1 (pre-2026-05-06) wrapped both directions in an outer postcard
//! envelope (`RpcEnvelope` for requests, `ResponseEnvelope` for
//! responses) carrying `payload_bytes: Vec<u8>`. That re-encoded the
//! typed body bytes a second time, costing a 64 KiB memcopy per call
//! on each side for bulk verbs (a measured 53% throughput regression
//! at 64 KiB GET vs gRPC). V2 hoists `request_id`, `verb_tag`, and
//! `status` into fixed-width / length-prefixed prefix fields and
//! writes the typed body bytes directly — no postcard wrapping
//! around the payload, no second memcopy. The verb's body is still
//! postcard-encoded (the typed PutObjectRequest, PutObjectResponse,
//! etc.), but at the codec boundary, not wrapped again at the wire.
//!
//! Pre-1.0 wire-format break per ADR-042 §13.2: peers running V1
//! see V2 frames as `UnsupportedVersion` and reject; operators wipe +
//! redeploy. No rolling upgrade across this bump.
//!
//! Version byte is its own value space distinct from ADR-041's raft
//! transport version.

use serde::{Deserialize, Serialize};

/// Wire-format version for the native TCP-framed binding. V2 lifts
/// `request_id` + `verb_tag` + `status` into the frame header and
/// drops the outer postcard envelope. See module docs for the layout.
pub const NATIVE_TCP_FRAMED_VERSION_V2: u8 = 2;

/// Backwards-compat alias. V1 is no longer accepted on the wire;
/// the constant exists so downstream callers that referenced the old
/// name still compile (see ADR-041's similar pattern). Equal to V2;
/// no actual V1 support remains in the codec.
pub const NATIVE_TCP_FRAMED_VERSION_V1: u8 = NATIVE_TCP_FRAMED_VERSION_V2;

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

/// Wire-level RPC envelope. ADR-042 §2.2.
///
/// Postcard-encoded inside the outer length-framed frame. Carries
/// per-call routing metadata. The inner request/response bodies are
/// in `payload_bytes`, also postcard-encoded (the typed request +
/// response types live in `kiseki_proto::v1::native`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcEnvelope {
    /// Unique-per-connection identifier. Multiplexes concurrent
    /// requests on one TCP connection (h2 stream-id moral
    /// equivalent). Wraps to zero on overflow; the per-connection
    /// outstanding-request map is bounded so wrap is cosmetic.
    pub request_id: u64,
    /// Verb identifier — the kiseki proto verb name in
    /// `snake_case`. The server-side dispatch table maps this to a
    /// `ServerImpl` inherent method. Stable wire identity; rename =
    /// version bump.
    pub verb_tag: String,
    /// Postcard-encoded request body. Server decodes against the
    /// type the dispatch table associates with `verb_tag`.
    pub payload_bytes: Vec<u8>,
}

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

/// Decoded request frame view (V2). Holds borrowed slices into the
/// caller's buffer — no copies. The verb's request body is what
/// remains after the header and is what the dispatch layer
/// postcard-decodes into the typed request struct.
#[derive(Debug)]
pub struct RequestFrameView<'a> {
    /// Echoed by the response so the client can demultiplex.
    pub request_id: u64,
    /// Verb identifier — caller maps to a dispatch entry. UTF-8;
    /// `verb_tag.as_bytes().len() <= u8::MAX`.
    pub verb_tag: &'a str,
    /// Postcard-encoded body — the typed request struct. Caller
    /// `postcard::from_bytes(body)` into the verb's request type.
    pub body: &'a [u8],
}

/// Decoded response frame view (V2). Borrowed slice; caller copies
/// if the body must outlive the underlying buffer.
#[derive(Debug)]
pub struct ResponseFrameView<'a> {
    /// Status byte mapped to ADR-042 §1.4.
    pub status: WireStatus,
    /// Echoed from the request so the client correlates.
    pub request_id: u64,
    /// Postcard-encoded body on `Ok`; UTF-8 reason string on error
    /// statuses (caller decodes via `String::from_utf8_lossy`).
    pub body: &'a [u8],
}

/// Maximum verb-tag length on the wire. `u8::MAX` (255) — every
/// kiseki verb name is ≤ 24 chars, so this is generous headroom.
const MAX_VERB_TAG_LEN: usize = u8::MAX as usize;

/// Minimum response body bytes (post-length-prefix): version u8 +
/// status u8 + request_id u64 = 10. Anything shorter is malformed.
/// Public so client-side hot-path readers can size their header
/// stack buffers exactly.
pub const RESPONSE_HEADER_LEN: usize = 1 + 1 + 8;

/// Minimum request body bytes: version u8 + request_id u64 +
/// verb_tag_len u8 = 10 (verb_tag itself can be 1..=255 bytes;
/// body 0..=N).
pub const REQUEST_HEADER_FIXED_LEN: usize = 1 + 8 + 1;

/// Build a request frame *header* (length prefix + version +
/// request_id + verb_tag header). Caller writes this small buffer
/// to the wire, THEN writes `body` in a second `write_all` —
/// avoiding the 64 KiB `extend_from_slice` memcopy that
/// [`encode_request_frame`] would perform.
///
/// Header layout: `[len u32 BE][ver u8][request_id u64 BE][verb_tag_len u8][verb_tag bytes]`.
///
/// `body_len` is the length of the body bytes the caller will write
/// next — it's used to compute the length prefix. The header
/// itself is at most 14 + 255 = 269 bytes; typical kiseki verbs
/// produce ~24-byte headers.
///
/// # Errors
/// Same as [`encode_request_frame`].
#[allow(clippy::missing_errors_doc)]
pub fn build_request_header(
    request_id: u64,
    verb_tag: &str,
    body_len: usize,
) -> Result<Vec<u8>, WireEncodeError> {
    let verb_bytes = verb_tag.as_bytes();
    if verb_bytes.len() > MAX_VERB_TAG_LEN {
        return Err(WireEncodeError::Postcard(format!(
            "verb_tag length {} exceeds wire cap {MAX_VERB_TAG_LEN}",
            verb_bytes.len()
        )));
    }
    let total_body_len = REQUEST_HEADER_FIXED_LEN + verb_bytes.len() + body_len;
    if total_body_len > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireEncodeError::Oversize {
            len: total_body_len,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    let mut out = Vec::with_capacity(4 + REQUEST_HEADER_FIXED_LEN + verb_bytes.len());
    out.extend_from_slice(
        &u32::try_from(total_body_len).unwrap_or(u32::MAX).to_be_bytes(),
    );
    out.push(NATIVE_TCP_FRAMED_VERSION_V2);
    out.extend_from_slice(&request_id.to_be_bytes());
    #[allow(clippy::cast_possible_truncation)]
    out.push(verb_bytes.len() as u8);
    out.extend_from_slice(verb_bytes);
    Ok(out)
}

/// Encode a complete request frame in a single allocation. Convenient
/// for tests and small-payload callers; hot-path callers prefer
/// [`build_request_header`] + a separate `write_all(body)` to avoid
/// the body memcopy (saves ~64 KiB per call at typical object sizes).
///
/// # Errors
/// `Oversize` if the resulting body exceeds [`NATIVE_TCP_FRAMED_MAX_BODY`].
/// `Postcard` if `verb_tag` is longer than [`MAX_VERB_TAG_LEN`].
#[allow(clippy::missing_errors_doc)]
pub fn encode_request_frame(
    request_id: u64,
    verb_tag: &str,
    body: &[u8],
) -> Result<Vec<u8>, WireEncodeError> {
    let mut out = build_request_header(request_id, verb_tag, body.len())?;
    out.extend_from_slice(body);
    Ok(out)
}

/// Decode a request frame from a buffer that already contains a
/// complete frame body (length-prefix-stripped). Returns borrowed
/// slices into `body` — no copies; caller owns the buffer's
/// lifetime.
///
/// Input layout: `[ver u8][request_id u64 BE][verb_tag_len u8][verb_tag bytes][body...]`.
///
/// # Errors
/// `Incomplete` / `UnsupportedVersion` / `Postcard` (for invalid
/// UTF-8 in the verb tag).
#[allow(clippy::missing_errors_doc)]
pub fn decode_request_frame(body: &[u8]) -> Result<RequestFrameView<'_>, WireDecodeError> {
    if body.len() < REQUEST_HEADER_FIXED_LEN {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: REQUEST_HEADER_FIXED_LEN,
        });
    }
    let version = body[0];
    if version != NATIVE_TCP_FRAMED_VERSION_V2 {
        return Err(WireDecodeError::UnsupportedVersion {
            version,
            supported: NATIVE_TCP_FRAMED_VERSION_V2,
        });
    }
    let request_id = u64::from_be_bytes(body[1..9].try_into().unwrap());
    let verb_tag_len = body[9] as usize;
    let verb_tag_end = REQUEST_HEADER_FIXED_LEN + verb_tag_len;
    if body.len() < verb_tag_end {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: verb_tag_end,
        });
    }
    let verb_tag_bytes = &body[REQUEST_HEADER_FIXED_LEN..verb_tag_end];
    let verb_tag = std::str::from_utf8(verb_tag_bytes)
        .map_err(|e| WireDecodeError::Postcard(format!("verb_tag not utf-8: {e}")))?;
    Ok(RequestFrameView {
        request_id,
        verb_tag,
        body: &body[verb_tag_end..],
    })
}

/// Compatibility shim: legacy callers that consumed the V1
/// `RpcEnvelope`. Decodes the V2 wire shape and constructs an
/// equivalent owned `RpcEnvelope`. The body bytes are copied into
/// `payload_bytes` — slower than [`decode_request_frame`]'s borrow,
/// kept for tests / non-hot-path callers.
#[allow(clippy::missing_errors_doc)]
pub fn decode_request_body(body: &[u8]) -> Result<RpcEnvelope, WireDecodeError> {
    let view = decode_request_frame(body)?;
    Ok(RpcEnvelope {
        request_id: view.request_id,
        verb_tag: view.verb_tag.to_string(),
        payload_bytes: view.body.to_vec(),
    })
}

/// Build a response frame *header* (length prefix + version +
/// status + request_id). Fixed 14 bytes — caller writes this small
/// buffer first, then `write_all(body)` separately so the body
/// avoids one full memcopy on the way to the kernel.
///
/// Header layout: `[len u32 BE][ver u8][status u8][request_id u64 BE]`.
///
/// # Errors
/// `Oversize` if the resulting frame would exceed
/// [`NATIVE_TCP_FRAMED_MAX_BODY`].
#[allow(clippy::missing_errors_doc)]
pub fn build_response_header(
    status: WireStatus,
    request_id: u64,
    body_len: usize,
) -> Result<[u8; 4 + RESPONSE_HEADER_LEN], WireEncodeError> {
    let total_body_len = RESPONSE_HEADER_LEN + body_len;
    if total_body_len > NATIVE_TCP_FRAMED_MAX_BODY {
        return Err(WireEncodeError::Oversize {
            len: total_body_len,
            cap: NATIVE_TCP_FRAMED_MAX_BODY,
        });
    }
    let mut header = [0u8; 4 + RESPONSE_HEADER_LEN];
    header[0..4].copy_from_slice(
        &u32::try_from(total_body_len).unwrap_or(u32::MAX).to_be_bytes(),
    );
    header[4] = NATIVE_TCP_FRAMED_VERSION_V2;
    header[5] = status as u8;
    header[6..14].copy_from_slice(&request_id.to_be_bytes());
    Ok(header)
}

/// Encode a complete response frame in a single allocation —
/// convenience wrapper for tests + small-payload paths. Hot-path
/// callers prefer [`build_response_header`] + a separate
/// `write_all(body)` to avoid the body memcopy.
///
/// # Errors
/// `Oversize` if the body exceeds [`NATIVE_TCP_FRAMED_MAX_BODY`].
#[allow(clippy::missing_errors_doc)]
pub fn encode_response_frame(
    status: WireStatus,
    request_id: u64,
    body: &[u8],
) -> Result<Vec<u8>, WireEncodeError> {
    let header = build_response_header(status, request_id, body.len())?;
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    Ok(out)
}

/// Decode a response frame body (length-prefix-stripped). Returns
/// borrowed slices into `body`.
///
/// Input layout: `[ver u8][status u8][request_id u64 BE][body...]`.
///
/// # Errors
/// `Incomplete` / `UnsupportedVersion` / `Postcard` (unknown status byte).
#[allow(clippy::missing_errors_doc)]
pub fn decode_response_frame(body: &[u8]) -> Result<ResponseFrameView<'_>, WireDecodeError> {
    if body.len() < RESPONSE_HEADER_LEN {
        return Err(WireDecodeError::Incomplete {
            have: body.len(),
            need: RESPONSE_HEADER_LEN,
        });
    }
    let version = body[0];
    if version != NATIVE_TCP_FRAMED_VERSION_V2 {
        return Err(WireDecodeError::UnsupportedVersion {
            version,
            supported: NATIVE_TCP_FRAMED_VERSION_V2,
        });
    }
    let status = WireStatus::from_u8(body[1]).ok_or_else(|| {
        WireDecodeError::Postcard(format!("unknown status byte 0x{:02x}", body[1]))
    })?;
    let request_id = u64::from_be_bytes(body[2..10].try_into().unwrap());
    Ok(ResponseFrameView {
        status,
        request_id,
        body: &body[RESPONSE_HEADER_LEN..],
    })
}

/// Compatibility shim — V1 returned `(WireStatus, &[u8])`. V2 also
/// carries `request_id`, but callers from before the V2 cutover only
/// consumed status + body. Use [`decode_response_frame`] for new code.
#[allow(clippy::missing_errors_doc)]
pub fn decode_response_body(body: &[u8]) -> Result<(WireStatus, &[u8]), WireDecodeError> {
    let view = decode_response_frame(body)?;
    Ok((view.status, view.body))
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
        // V2 (post-2026-05-06) is the active wire version after the
        // request_id-hoist refactor that closed the bulk-payload
        // memcopy tax. V1's alias still equals V2 — the constant is
        // for downstream-name compat only; no V1 wire support
        // remains.
        assert_eq!(NATIVE_TCP_FRAMED_VERSION_V2, 2);
        assert_eq!(NATIVE_TCP_FRAMED_VERSION_V1, NATIVE_TCP_FRAMED_VERSION_V2);
    }

    #[test]
    fn reserved_version_bytes_match_json_starts() {
        // `[` `{` `"` — JSON document starts. Pin so a refactor
        // doesn't inadvertently assign a version byte that overlaps.
        assert_eq!(RESERVED_VERSION_BYTES, [0x5B, 0x7B, 0x22]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V1, RESERVED_VERSION_BYTES[0]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V1, RESERVED_VERSION_BYTES[1]);
        assert_ne!(NATIVE_TCP_FRAMED_VERSION_V1, RESERVED_VERSION_BYTES[2]);
    }

    #[test]
    fn rpc_envelope_postcard_roundtrip() {
        let env = RpcEnvelope {
            request_id: 0xDEAD_BEEF_CAFE_F00D,
            verb_tag: "put_object".into(),
            payload_bytes: vec![0xAA; 64],
        };
        let bytes = postcard::to_allocvec(&env).expect("encode");
        let decoded: RpcEnvelope = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(env, decoded);
    }

    #[test]
    fn request_frame_roundtrip() {
        let body = b"verb-body-bytes";
        let framed = encode_request_frame(42, "get_object", body).expect("encode");
        // Skip the 4-byte length prefix; decoder operates on body.
        assert!(framed.len() > 4);
        let length =
            u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        let frame_body = &framed[4..];
        assert_eq!(frame_body.len(), length);
        let view = decode_request_frame(frame_body).expect("decode");
        assert_eq!(view.request_id, 42);
        assert_eq!(view.verb_tag, "get_object");
        assert_eq!(view.body, body);
    }

    #[test]
    fn request_frame_starts_with_supported_version_byte() {
        let framed = encode_request_frame(0, "x", &[]).expect("encode");
        // Layout: [len u32 BE][ver u8][...]
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V2);
    }

    #[test]
    fn decode_rejects_unsupported_version() {
        // Construct a body with version byte 99 — never supported.
        let mut body = vec![99u8];
        body.extend_from_slice(&[0; 9]);
        let err = decode_request_frame(&body).expect_err("must reject");
        match err {
            WireDecodeError::UnsupportedVersion { version, supported } => {
                assert_eq!(version, 99);
                assert_eq!(supported, NATIVE_TCP_FRAMED_VERSION_V2);
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
        // Build a frame with verb_tag bytes that aren't valid UTF-8.
        let mut frame_body = vec![NATIVE_TCP_FRAMED_VERSION_V2];
        frame_body.extend_from_slice(&0u64.to_be_bytes()); // request_id
        frame_body.push(2); // verb_tag_len
        frame_body.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
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
        let payload = b"some-postcard-bytes";
        let framed = encode_response_frame(WireStatus::Ok, 99, payload).expect("encode");
        let body = &framed[4..];
        let view = decode_response_frame(body).expect("decode");
        assert_eq!(view.status, WireStatus::Ok);
        assert_eq!(view.request_id, 99);
        assert_eq!(view.body, payload);
    }

    #[test]
    fn response_frame_roundtrip_error_status() {
        let payload = b"reason: san_payload_tenant_mismatch";
        let framed = encode_response_frame(WireStatus::PermissionDenied, 7, payload)
            .expect("encode");
        let body = &framed[4..];
        let view = decode_response_frame(body).expect("decode");
        assert_eq!(view.status, WireStatus::PermissionDenied);
        assert_eq!(view.request_id, 7);
        assert_eq!(view.body, payload);
    }

    #[test]
    fn response_frame_short_body_is_incomplete() {
        // Only one byte (version) — missing status + request_id.
        let body = vec![NATIVE_TCP_FRAMED_VERSION_V2];
        let err = decode_response_frame(&body).expect_err("must reject");
        assert!(matches!(err, WireDecodeError::Incomplete { .. }));
    }

    #[test]
    fn response_frame_unknown_status_byte_rejected() {
        let mut body = vec![NATIVE_TCP_FRAMED_VERSION_V2, 0x77];
        body.extend_from_slice(&[0; 8]); // request_id padding
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
        let err = encode_request_frame(0, "x", &body).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Oversize { .. }));
    }

    #[test]
    fn encode_response_frame_oversize_rejected() {
        let payload = vec![0u8; NATIVE_TCP_FRAMED_MAX_BODY];
        let err = encode_response_frame(WireStatus::Ok, 0, &payload).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Oversize { .. }));
    }

    #[test]
    fn encode_request_with_oversize_verb_tag_rejected() {
        // Verb tag length is u8 on the wire — 256 bytes overflows.
        let big_verb = "x".repeat(256);
        let err = encode_request_frame(0, &big_verb, &[]).expect_err("must reject");
        assert!(matches!(err, WireEncodeError::Postcard(_)));
    }

    /// End-to-end framing layout assertion — proves the wire bytes
    /// match the V2 layout so a refactor that changes either side
    /// fails the test rather than silently breaking peers.
    #[test]
    fn request_frame_layout_matches_spec() {
        let body = b"\xCA\xFE";
        let framed = encode_request_frame(7, "ping", body).expect("encode");
        // [len u32 BE]
        let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(len, framed.len() - 4);
        // [ver u8]
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V2);
        // [request_id u64 BE]
        let request_id = u64::from_be_bytes(framed[5..13].try_into().unwrap());
        assert_eq!(request_id, 7);
        // [verb_tag_len u8]
        assert_eq!(framed[13], 4); // "ping" is 4 bytes
        // [verb_tag bytes]
        assert_eq!(&framed[14..18], b"ping");
        // [body]
        assert_eq!(&framed[18..], body);
    }

    /// Response frame layout pin.
    #[test]
    fn response_frame_layout_matches_spec() {
        let body = b"\xDE\xAD\xBE\xEF";
        let framed = encode_response_frame(WireStatus::Ok, 13, body).expect("encode");
        let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(len, framed.len() - 4);
        assert_eq!(framed[4], NATIVE_TCP_FRAMED_VERSION_V2);
        assert_eq!(framed[5], WireStatus::Ok as u8);
        let request_id = u64::from_be_bytes(framed[6..14].try_into().unwrap());
        assert_eq!(request_id, 13);
        assert_eq!(&framed[14..], body);
    }
}
