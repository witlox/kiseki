# Adversary Gate-1 Round-2 Findings — ADR-042 (post-amendment review)

**Type**: Adversary → Architect → Implementer (second-pass verification)
**Date**: 2026-05-05
**Reviewer**: adversary (architecture mode)
**Mode**: re-review of the gate-1 amendments to ADR-042 + the new I-NG15..I-NG24, A-NG21..A-NG24, F-NG12..F-NG13. Looking for issues introduced *by* the resolutions, not just whether they cover the original findings.
**Verdict**: **PASS — minor non-blocking fixes.** 0 CRITICAL, 1 HIGH (implementation-level), 5 MEDIUM, 2 LOW. The 2 CRITICAL + 6 HIGH gate-1 findings are all properly resolved at the spec layer. Residual issues are detail-level and don't gate the implementer.

## Coverage of original gate-1 findings

| Original | Resolution location | Verified? |
|---|---|---|
| F-C1 | ADR-042 §9.1 + I-NG15 | ✓ four named keys, HKDF salts, master-key-epoch, rotation grace window, compromise model spelled out |
| F-C2 | ADR-042 §5.0 + I-NG16 + F-NG12 | ✓ all-or-nothing barrier, parallelism knob, partial-failure mode + scrub recovery |
| F-H1 | HandleToken `cert_san_canonical` field + I-NG17 + proto comment update | ✓ token bound to issuing cert SAN |
| F-H2 | `BatchFetchDek` RPC + A-NG21 + F-NG13 (latency-cliff regression captured) | ✓ single round-trip per Read |
| F-H3 | `GetTopologyRequest.tenant_id` + I-NG20 | ✓ tenant-scoped shards |
| F-H4 | ADR-042 §6 ordering + I-NG18 | ✓ fencing check before dedup |
| F-H5 | ADR-042 §10 policy + I-NG19 | ✓ explicit reject-vs-substitute branches |
| F-H6 | ADR-042 §12 DashMap + counter sequence | ✓ but introduces a new HIGH (N1, below) |
| F-M1..F-M8 | various | ✓ all eight addressed |
| F-L1..F-L4 | deferred to implementation | ✓ accepted as not gating |

All 16 substantive original findings cleared. Net new issues from the resolutions:

---

## NEW HIGH (implementation-level — informational for implementer)

### N1: Per-tenant stream counter leaks if stream-close path fails

**Severity**: High (correctness; implementation-level)
**Category**: Robustness > Resource exhaustion
**Location**: ADR-042 §12 ("DashMap... fetch_add(1, Acquire)... fetch_sub(1, Release) on overflow")

**Description**: The `fetch_add` claim + `fetch_sub` rollback on cap overflow is correct under normal flow. But the long-lived in-flight counter — incremented when a stream opens, decremented when it closes — can leak if the close path doesn't run:

- Future is dropped before `Drop` of the stream handler runs (panic, abort, runtime shutdown).
- Server-side error path that early-returns before reaching the decrement site.
- Cancellation safety: tonic streams can be cancelled mid-flight; the cancellation path must decrement.

A leaked counter slot permanently consumes one of the per-tenant cap N slots. Over time a tenant's effective cap shrinks until they can't open any new streams.

**Suggested resolution**: implementer wraps the counter slot in an RAII Drop guard:

```rust
struct StreamSlot {
    counter: Arc<AtomicUsize>,
}

impl StreamSlot {
    fn try_acquire(counter: Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        let prev = counter.fetch_add(1, Acquire);
        if prev >= cap {
            counter.fetch_sub(1, Release);
            None
        } else {
            Some(Self { counter })
        }
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Release);
    }
}
```

The handler holds the `StreamSlot` for the duration of the stream; on any unwind (panic, drop, normal completion) the Drop fires and decrements correctly.

This is HIGH because the bug is silent: a leaking counter manifests as "tenant occasionally can't open streams for no reason," easy to misdiagnose. Architect should mention the RAII discipline in §12; implementer must follow it.

---

## NEW MEDIUM

### N2: `BatchFetchDek` request size limit + handling > 1024 chunks

**Severity**: Medium
**Category**: Specification > Edge cases
**Location**: ADR-042 §1 (RPC signature), proto schema (BatchFetchDekRequest comment "up to 1024 per request")

**Description**: The proto comment caps the request at 1024 tickets. But:
- A 4 GiB Read in TrustedCompute (with 4 MiB chunks) has 1024 chunks — borderline.
- A 64 GiB Read has 16 384 chunks — exceeds.
- Spec doesn't say what happens at > 1024: server rejects? Server processes the first 1024 silently? Client must split into multiple BatchFetchDek calls?

The keymanager call would parallelize anyway via tonic streaming, but the *batching contract* is unclear.

**Suggested resolution**: ADR-042 §8 explicit clause: "Server enforces `BATCH_FETCH_DEK_MAX_TICKETS = 1024` per request; excess returns `InvalidArgument{batch_too_large, max=1024}`. Clients with > 1024 chunks per Read split into successive BatchFetchDek calls; the per-Read latency budget is dominated by the FIRST batch since subsequent batches overlap with chunk decryption."

### N3: `workflow_ref` policy — does it apply to S3 / NFS / FUSE paths too?

**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: ADR-042 §10 (workflow_ref_required_for_writes), I-NG19, A-NG6 amendment

**Description**: The new policy field `workflow_ref_required_for_writes` is on the **tenant** (per ADR-020). Existing S3 / NFS / FUSE code paths *also* honor `workflow_ref` (S3 via `x-kiseki-workflow-ref` header). The spec doesn't say whether the new policy is:

- (a) Path-agnostic: any tenant flagged `required` rejects writes-without-workflow-ref on ALL paths (S3, NFS, FUSE, native). This is the cleanest model but is a behavioral change for existing S3 / NFS clients.
- (b) Native-only: the policy flag is a new field that only the native path consults. Existing paths keep their current behavior. Confusing — same tenant can have different attribution semantics by path.

**Suggested resolution**: ADR-042 §10 specify (a) — path-agnostic. Tenant policy applies uniformly. Add a migration note: existing tenants default to `workflow_ref_required_for_writes = false`, so behavior is unchanged unless an operator flips the flag.

### N4: Multipart `upload_id` format undefined despite being signed

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §9.1 (multipart_upload_signing_key), proto schema (`string upload_id`)

**Description**: §9.1 lists `multipart_upload_signing_key` as one of the four signing keys. But the proto schema has `string upload_id` with no format specification. What does the gateway sign? What does the server validate?

A consistent model:
- `upload_id` is base64-url-encoded `[1 byte schema_version][postcard MultipartUploadToken][HMAC-SHA256 tag]`.
- `MultipartUploadToken { tenant_id, namespace_id, name, started_at, issuance_nonce }`.
- Server verifies the HMAC on every `PutPart` / `CompleteMultipart` / `AbortMultipart`.

This prevents:
- Cross-tenant upload_id replay (token binds to tenant).
- Forging upload_ids to attack the staging buffer.

**Suggested resolution**: add ADR-042 §5.1 *Multipart upload_id format* specifying the structure. Update the proto comment on `string upload_id` to reference §5.1. Without this, the implementer either over-engineers (server-side state map of upload_ids) or under-engineers (treat upload_id as opaque string, no auth).

### N5: `topology_signing_key` reserved-but-unused

**Severity**: Medium
**Category**: Specification > Specification compliance
**Location**: ADR-042 §9.1 (table)

**Description**: §9.1 lists `topology_signing_key` as "reserved, future use" but doesn't say what for. Reserved-but-unused entries in a discipline table invite silent reuse later (someone derives it for a different purpose without re-deriving from the table).

**Suggested resolution**: either remove the row (clean), or specify what it would sign (e.g., "future: signing of topology_version values to detect tampering on untrusted client devices"). Keeping placeholder rows in security tables is a code smell.

### N6: I-NG17 cert revocation has a 60s window not referenced

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Specification compliance
**Location**: I-NG17 ("Cert revocation invalidates handle tokens implicitly")

**Description**: I-NG17 says cert revocation invalidates handle tokens because the revoked cert can't establish a new mTLS connection. True, but: A-NG17 (cert revocation mid-session) accepts a 60s window between revocation and the gateway's periodic re-validation tearing down active streams (`KISEKI_CERT_REVAL_INTERVAL_MS` default). During that window, a held handle token CAN still be presented over the existing live connection.

I-NG17's bare claim "implicitly invalidates" is correct in the long run but obscures the 60s exposure window.

**Suggested resolution**: amend I-NG17: "Cert revocation invalidates handle tokens within the cert re-validation window (per A-NG17, default ≤60 s). Long-running streams are torn down on re-validation; new connections are rejected at the mTLS handshake."

---

## NEW LOW

### L1: `KISEKI_PUT_CHUNK_PARALLELISM` default 4 unmotivated

**Severity**: Low
**Category**: Specification

**Description**: ADR-042 §5.0 picks 4 as the default chunk-write parallelism for multi-chunk PUTs. Why 4? Larger objects benefit from higher concurrency; smaller systems benefit from less. The number is plausible but uncited.

**Suggested resolution**: add a one-line rationale: "Default 4 keeps per-PUT memory bounded at `KISEKI_PUT_CHUNK_PARALLELISM × MAX_PLAINTEXT_PER_CHUNK = 16 MiB` per in-flight PUT; operators with high-bandwidth fabrics raise it." Or pick a different default and justify.

### L2: `GetTopology` empty-shard-list for callers with no namespaces

**Severity**: Low
**Category**: Specification > Edge cases

**Description**: A caller authenticated with a valid tenant cert but who has no namespaces in any shard (e.g., a freshly-onboarded tenant before namespace creation): `GetTopology` returns empty `shards`. Spec doesn't clarify whether this is normal behavior or an error.

**Suggested resolution**: spec: "Empty `shards` is normal for a tenant with no live namespaces. Clients SHOULD treat this as 'no work to route'." No error. Trivial clarification.

---

## Summary

The amendments resolve all original gate-1 findings cleanly. The residual issues are:
- **N1 (HIGH)** is a genuine implementation concern but standard Rust RAII solves it; architect should mention the discipline in §12.
- **N2..N6 (MEDIUM)** are detail-level spec gaps that resolve in <1 hour each of architect time. None block the implementer if marked as TODOs.
- **L1, L2 (LOW)** are clarity nits.

**Recommendation**: architect can fold N1's RAII guidance into §12 + N2..N5 brief amendments in the same commit, or punt to implementer with these findings as the work list. Either way, **proceed to implementer**. The 2 CRITICAL + 6 HIGH gate-1 issues are properly resolved; nothing in this round blocks the implementer from beginning.

The implementer should treat:
- N1 as a **hard requirement**: RAII Drop guard around the StreamSlot. Catches a real silent failure mode.
- N2 (BatchFetchDek 1024 limit), N3 (workflow_ref policy scope), N4 (multipart upload_id format), N5 (topology_signing_key disposition), N6 (I-NG17 revocation window): clarify in spec OR pick a sensible default and document it as a code comment that references the gap.
- L1, L2: cosmetic; don't gate.
