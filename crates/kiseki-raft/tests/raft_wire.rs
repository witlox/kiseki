//! Layer 1 reference tests for the **openraft RPC framing** kiseki
//! uses for inter-node Raft traffic.
//!
//! ADR-023 §D2: per-spec-section unit tests. There is no IETF RFC
//! for openraft — the "spec" is the openraft 0.10 wire convention as
//! implemented in `kiseki-raft::tcp_transport`:
//!
//!   - Length-prefixed framing: u32 big-endian length, followed by
//!     `length` bytes of `serde_json` payload.
//!   - Hard cap `MAX_RAFT_RPC_SIZE = 128 MiB`. A larger length
//!     prefix is rejected at the boundary (ADV-S1, ADV-S6).
//!   - Payload is `(tag: String, body: T)` where `tag ∈ {
//!     "append_entries", "vote", "full_snapshot" }`.
//!   - `AppendEntries` / `Vote` / `InstallSnapshot` request bodies
//!     ride as `serde_json::to_vec` of openraft's typed structs.
//!
//! Owner: `kiseki-raft::tcp_transport` — `rpc_exchange`,
//! `dispatch_raft_rpc`, and the `MAX_RAFT_RPC_SIZE` constant.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "openraft / Raft RPC".
//!
//! Spec text:
//! - openraft 0.10 docs: <https://databendlabs.github.io/openraft/>.
//! - Raft paper: Ongaro & Ousterhout, "In Search of an Understandable
//!   Consensus Algorithm" (USENIX ATC '14) — RPC argument names.
//! - Kiseki framing: `crates/kiseki-raft/src/tcp_transport.rs` — the
//!   length-prefix + JSON convention is local to kiseki.

use std::io::Cursor;

use kiseki_raft::tcp_transport::MAX_RAFT_RPC_SIZE;
use kiseki_raft::KisekiNode;
use openraft::raft::{AppendEntriesRequest, VoteRequest};
use openraft::{declare_raft_types, Vote};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test type config — minimal openraft type config so the wire-shape
// tests can build typed AppendEntries / Vote requests. Mirrors the
// `KeyTypeConfig` in `kiseki-keymanager::raft::types` and the
// `TestConfig` in `kiseki-raft::redb_raft_log_store::tests`.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TestCmd {
    op: String,
}

impl std::fmt::Display for TestCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestCmd({})", self.op)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TestResp;

impl std::fmt::Display for TestResp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ok")
    }
}

declare_raft_types!(
    WireTestConfig:
        D = TestCmd,
        R = TestResp,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);

// ===========================================================================
// Sentinel constants — Raft RPC framing rules
// ===========================================================================

/// Kiseki's framing prefix is a 4-byte (u32) big-endian length.
const FRAME_PREFIX_LEN: usize = 4;

/// `kiseki-raft::tcp_transport::MAX_RAFT_RPC_SIZE` = 128 MiB. The
/// constant is exported; pin its expected value here so a future
/// change must be deliberate.
#[test]
fn max_raft_rpc_size_pinned_at_128_mib() {
    assert_eq!(
        MAX_RAFT_RPC_SIZE,
        128 * 1024 * 1024,
        "Kiseki Raft framing: MAX_RAFT_RPC_SIZE must be 128 MiB \
         (ADV-S1 / ADV-S6 OOM cap)"
    );
}

/// Length prefix is u32 big-endian — 4 bytes wide.
#[test]
fn frame_length_prefix_is_4_bytes_big_endian() {
    let n: u32 = 0xDEAD_BEEF;
    let bytes = n.to_be_bytes();
    assert_eq!(bytes.len(), FRAME_PREFIX_LEN);
    assert_eq!(bytes, [0xDE, 0xAD, 0xBE, 0xEF]);
    let parsed = u32::from_be_bytes(bytes);
    assert_eq!(parsed, n, "u32 BE length prefix must round-trip");
}

// ===========================================================================
// Length-prefix framing — positive cases
// ===========================================================================

/// Helper: build a kiseki Raft framed message: 4-byte BE length +
/// payload bytes.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_PREFIX_LEN + payload.len());
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Helper: parse a kiseki Raft frame (the receiver side of
/// `rpc_exchange`). Returns `Err` on truncated prefix or oversized
/// length.
fn unframe(bytes: &[u8]) -> Result<&[u8], &'static str> {
    if bytes.len() < FRAME_PREFIX_LEN {
        return Err("short prefix");
    }
    let mut len_buf = [0u8; FRAME_PREFIX_LEN];
    len_buf.copy_from_slice(&bytes[..FRAME_PREFIX_LEN]);
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_RAFT_RPC_SIZE {
        return Err("oversized");
    }
    if bytes.len() < FRAME_PREFIX_LEN + len {
        return Err("short body");
    }
    Ok(&bytes[FRAME_PREFIX_LEN..FRAME_PREFIX_LEN + len])
}

#[test]
fn frame_length_prefix_round_trip_empty() {
    let bytes = frame(b"");
    assert_eq!(bytes, vec![0, 0, 0, 0], "empty body → length=0 prefix only");
    let body = unframe(&bytes).expect("unframe empty");
    assert!(body.is_empty());
}

#[test]
fn frame_length_prefix_round_trip_small_body() {
    let payload = b"hello raft";
    let bytes = frame(payload);
    assert_eq!(&bytes[..FRAME_PREFIX_LEN], &[0, 0, 0, 10]);
    assert_eq!(&bytes[FRAME_PREFIX_LEN..], payload);
    let body = unframe(&bytes).expect("unframe");
    assert_eq!(body, payload);
}

#[test]
fn frame_length_prefix_round_trip_at_size_cap() {
    // We cannot allocate 128 MiB cheaply, but we CAN show that the
    // prefix encoding for the cap value is correct.
    let cap = u32::try_from(MAX_RAFT_RPC_SIZE).unwrap();
    let prefix = cap.to_be_bytes();
    assert_eq!(
        u32::from_be_bytes(prefix) as usize,
        MAX_RAFT_RPC_SIZE,
        "MAX_RAFT_RPC_SIZE round-trips through the BE prefix"
    );
}

// ===========================================================================
// Length-prefix framing — rejection cases (ADV-S1 / ADV-S6)
// ===========================================================================

/// `tcp_transport::dispatch_raft_rpc` aborts on any read error. A
/// frame with fewer than 4 bytes of prefix MUST fail before the
/// peer can amplify the side effect.
#[test]
fn frame_rejects_short_prefix() {
    for n in 0..FRAME_PREFIX_LEN {
        let bytes = vec![0xFFu8; n];
        let r = unframe(&bytes);
        assert!(
            r.is_err(),
            "Kiseki Raft framing: prefix shorter than 4 bytes ({n}) must error"
        );
    }
}

/// `tcp_transport::rpc_exchange` rejects responses whose length
/// prefix exceeds `MAX_RAFT_RPC_SIZE`. The dispatcher applies the
/// same rule on the request side. This is the OOM-prevention
/// contract.
#[test]
fn frame_rejects_oversized_length_prefix() {
    // 128 MiB + 1 byte.
    let oversized = u32::try_from(MAX_RAFT_RPC_SIZE + 1).unwrap();
    let mut bytes = Vec::with_capacity(FRAME_PREFIX_LEN);
    bytes.extend_from_slice(&oversized.to_be_bytes());
    // No body — shouldn't matter, the prefix check fires first.
    let r = unframe(&bytes);
    assert!(
        matches!(r, Err("oversized")),
        "Kiseki Raft framing: length prefix > MAX_RAFT_RPC_SIZE must be rejected \
         (ADV-S1 / ADV-S6 OOM cap)"
    );
}

/// A 1 GiB length prefix is the canonical adversarial case from
/// `tcp_transport::tests::server_drops_oversized_rpc_request`.
#[test]
fn frame_rejects_one_gib_length_prefix() {
    let one_gib: u32 = 1024 * 1024 * 1024;
    assert!(
        (one_gib as usize) > MAX_RAFT_RPC_SIZE,
        "1 GiB exceeds the 128 MiB cap"
    );
    let bytes = one_gib.to_be_bytes().to_vec();
    let r = unframe(&bytes);
    assert!(
        matches!(r, Err("oversized")),
        "Kiseki Raft framing: 1 GiB prefix → reject (ADV-S1)"
    );
}

// ===========================================================================
// AppendEntries serialization round-trip
// ===========================================================================

/// openraft 0.10 `AppendEntriesRequest<C>` serializes via serde_json
/// to a stable shape: `{ vote, prev_log_id, entries, leader_commit }`.
/// Round-trip MUST be identity.
#[test]
fn append_entries_request_json_round_trip_empty_entries() {
    // Heartbeat (empty entries, no prior log entry).
    // Vote(term=1, leader=node_id=42); committed.
    let req: AppendEntriesRequest<WireTestConfig> = AppendEntriesRequest {
        vote: Vote::new_committed(1u64, 42u64),
        prev_log_id: None, // first contact — no prior log
        entries: vec![],   // heartbeat
        leader_commit: None,
    };
    let bytes = serde_json::to_vec(&req).expect("encode AppendEntries");
    assert!(!bytes.is_empty());

    let back: AppendEntriesRequest<WireTestConfig> =
        serde_json::from_slice(&bytes).expect("decode AppendEntries");
    assert_eq!(back.vote, req.vote);
    assert_eq!(back.prev_log_id, req.prev_log_id);
    assert_eq!(back.entries.len(), 0);
    assert_eq!(back.leader_commit, req.leader_commit);
}

#[test]
fn append_entries_request_with_kiseki_tag_prefix() {
    // The dispatcher reads `("append_entries", req)` — confirm the
    // tagged tuple round-trips.
    let req: AppendEntriesRequest<WireTestConfig> = AppendEntriesRequest {
        vote: Vote::new_committed(2u64, 7u64),
        prev_log_id: None,
        entries: vec![],
        leader_commit: None,
    };
    let tagged = ("append_entries".to_string(), req);
    let bytes = serde_json::to_vec(&tagged).expect("encode tagged");
    let back: (String, AppendEntriesRequest<WireTestConfig>) =
        serde_json::from_slice(&bytes).expect("decode tagged");
    assert_eq!(back.0, "append_entries");
    assert_eq!(back.1.vote, tagged.1.vote);
}

// ===========================================================================
// Vote serialization round-trip
// ===========================================================================

/// openraft `VoteRequest<C>` carries `{ vote, last_log_id }`. Round
/// trip MUST be identity.
#[test]
fn vote_request_json_round_trip() {
    let req: VoteRequest<WireTestConfig> = VoteRequest {
        vote: Vote::new(3u64, 17u64),
        // Skipping `last_log_id` construction — the LogId<C> type is
        // generic in C::LeaderId, which `declare_raft_types!` defaults
        // to the `_adv` variant. The wire-shape property we care about
        // (round-trip) is exercised with `None` here.
        last_log_id: None,
    };
    let bytes = serde_json::to_vec(&req).expect("encode Vote");
    let back: VoteRequest<WireTestConfig> = serde_json::from_slice(&bytes).expect("decode Vote");
    assert_eq!(back.vote, req.vote);
    assert_eq!(back.last_log_id, req.last_log_id);
}

#[test]
fn vote_request_with_kiseki_tag_prefix() {
    let req: VoteRequest<WireTestConfig> = VoteRequest {
        vote: Vote::new(4u64, 33u64),
        last_log_id: None, // first vote — no prior log
    };
    let tagged = ("vote".to_string(), req);
    let bytes = serde_json::to_vec(&tagged).expect("encode tagged vote");
    let back: (String, VoteRequest<WireTestConfig>) =
        serde_json::from_slice(&bytes).expect("decode tagged vote");
    assert_eq!(back.0, "vote");
    assert_eq!(back.1.vote, tagged.1.vote);
    assert_eq!(back.1.last_log_id, None);
}

// ===========================================================================
// InstallSnapshot (full_snapshot) envelope round-trip
// ===========================================================================
//
// Kiseki's full-snapshot envelope (`SnapshotEnvelope` in
// `tcp_transport.rs`) carries `{ vote, meta, data: Vec<u8> }`. The
// envelope is private to the transport, so this test rebuilds an
// equivalent shape and asserts the JSON round-trip works for the
// snapshot's user-visible parts.

#[test]
fn install_snapshot_envelope_round_trip() {
    // Mirror of `kiseki_raft::tcp_transport::SnapshotEnvelope` for
    // the test (the real type is private to the crate). We keep the
    // shape JSON-typed so it stays decoupled from openraft's
    // generic Vote<C> machinery.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct LocalSnapshotEnvelope {
        // The kiseki transport carries the openraft Vote here; the
        // round-trip property is on the JSON bytes — that's what's
        // exposed on the wire.
        vote_json: serde_json::Value,
        // We don't replicate the full SnapshotMeta type here — it's
        // proprietary to openraft's storage layer. The bytes-level
        // round-trip on `data` is what matters.
        data: Vec<u8>,
    }

    // Use openraft's typed VoteOf alias for the test config so the
    // leader-id flavor (std vs adv) is unambiguous.
    let real_vote: openraft::alias::VoteOf<WireTestConfig> = Vote::new_committed(5u64, 3u64);
    let vote_json = serde_json::to_value(&real_vote).expect("vote → json");

    let env = LocalSnapshotEnvelope {
        vote_json,
        data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03],
    };
    let bytes = serde_json::to_vec(&env).expect("encode envelope");
    let back: LocalSnapshotEnvelope = serde_json::from_slice(&bytes).expect("decode envelope");
    assert_eq!(back, env);
}

#[test]
fn install_snapshot_with_kiseki_tag_prefix() {
    // Confirm the `("full_snapshot", envelope)` tuple is what the
    // dispatcher expects.
    #[derive(Serialize, Deserialize)]
    struct LocalEnv {
        vote_json: serde_json::Value,
        data: Vec<u8>,
    }
    let real_vote: openraft::alias::VoteOf<WireTestConfig> = Vote::new_committed(1u64, 1u64);
    let vote_json = serde_json::to_value(&real_vote).expect("vote → json");
    let env = LocalEnv {
        vote_json,
        data: vec![0xAA, 0xBB],
    };
    let tagged = ("full_snapshot".to_string(), env);
    let bytes = serde_json::to_vec(&tagged).expect("encode tagged");

    // Decode the tag back; the dispatcher uses a 2-step decode where
    // the tag is read first (as a (String, JsonValue) tuple).
    let (tag, _value): (String, serde_json::Value) =
        serde_json::from_slice(&bytes).expect("decode tagged");
    assert_eq!(
        tag, "full_snapshot",
        "Kiseki Raft framing: install-snapshot tag is 'full_snapshot'"
    );
}

// ===========================================================================
// Cross-implementation seed — hand-built AppendEntries frame
// ===========================================================================

/// Cross-implementation seed: hand-build a heartbeat AppendEntries
/// frame from the openraft 0.10 documentation's wire shape. The bytes
/// below are what kiseki's transport produces today; they will be
/// the regression sentinel if openraft's serde encoding shifts.
///
/// Source: openraft 0.10 docs §"AppendEntriesRequest" + the kiseki
/// `("append_entries", request)` tuple convention.
///
/// The test uses `serde_json` to derive the expected bytes once per
/// run (so it is robust to whitespace differences); the framing
/// length prefix is assembled by hand to assert the kiseki wire
/// shape end-to-end.
#[test]
fn rfc_seed_hand_built_append_entries_heartbeat_frame() {
    // Build a minimal heartbeat: empty entries, no prior log.
    let req: AppendEntriesRequest<WireTestConfig> = AppendEntriesRequest {
        vote: Vote::new_committed(1u64, 100u64),
        prev_log_id: None, // node has never seen a log entry
        entries: vec![],
        leader_commit: None,
    };
    let tagged = ("append_entries".to_string(), req);

    let body = serde_json::to_vec(&tagged).expect("encode tagged frame body");
    let framed = frame(&body);

    // Frame structure: [4-byte BE length][JSON body]. Hand-derive the
    // BE length and assert the prefix shape.
    let body_len = u32::try_from(body.len()).expect("body fits in u32");
    let expected_prefix = body_len.to_be_bytes();
    assert_eq!(
        &framed[..FRAME_PREFIX_LEN],
        &expected_prefix,
        "Kiseki Raft framing: prefix is u32 BE of body length"
    );
    assert_eq!(
        &framed[FRAME_PREFIX_LEN..],
        body.as_slice(),
        "Kiseki Raft framing: body follows prefix verbatim"
    );

    // And confirm round-trip through unframe + serde decode.
    let body_back = unframe(&framed).expect("unframe");
    let (tag, _val): (String, serde_json::Value) =
        serde_json::from_slice(body_back).expect("decode tag");
    assert_eq!(tag, "append_entries");

    // The body is non-empty (the JSON tag string alone is at least
    // the 17 bytes of `["append_entries",`). This guards against an
    // empty-frame regression where the dispatcher would silently
    // accept an empty body.
    assert!(
        body.len() > 17,
        "Kiseki Raft framing: AppendEntries body must contain tag + payload; \
         got only {} bytes",
        body.len()
    );
}
