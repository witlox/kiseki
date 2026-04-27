# Adversary Gate 1 Request — ADR-038 (pNFS layout + DS subprotocol)

**Type**: Architect → Adversary
**Date**: 2026-04-27
**Author**: architect
**Status**: open — block implementation until cleared

## Artifacts to review

1. `specs/architecture/adr/038-pnfs-layout-and-ds-subprotocol.md`
2. `specs/architecture/data-models/pnfs.rs` (interface stubs)
3. `specs/invariants.md` — new section "pNFS invariants (ADR-038)" — I-PN1..I-PN7
4. `specs/architecture/enforcement-map.md` — pNFS section (I-PN1..I-PN7)
5. `specs/architecture/api-contracts.md` — Protocol Gateway (NFS) update
6. `specs/architecture/build-phases.md` — Phase 15a/b/c

## Why this needs gate 1

ADR-038 introduces:

- A new externally-reachable network listener (`ds_addr`, default `:2052`)
  on every storage node, terminating mTLS and answering an NFSv4.1 op
  subset. **New attack surface.**
- A self-authenticating file handle scheme (HMAC over
  `tenant‖ns‖comp‖stripe‖expiry`) where forged or replayed fh4s
  must be rejected. **New crypto contract.**
- Cross-context event subscriptions: ADR-033 split/merge, ADR-034
  merge, ADR-035 drain → fire LAYOUTRECALL within 1s. **New SLA
  with multi-context blast radius.**
- Tight-coupled pNFS state ownership where DS is stateless and MDS
  owns all opens/locks/layouts. Recovery semantics inherited from
  NFSv4.1 session reclaim. **Failure-mode reasoning required.**

## Specific questions for adversary

1. **fh4 forgery**: HMAC-SHA256 truncated to 16 bytes (128 bits) over
   the listed fields. Is the field order canonical and unambiguous? Is
   16-byte MAC sufficient given the rate at which a malicious client
   could probe? (I-PN1)
2. **fh4 replay across MAC-key-rotation boundary**: I-PN4 says
   layouts have ≤5 min TTL. After fh4 MAC key rotation, do we have a
   gap where old fh4s remain valid? Does I-PN5 close it?
3. **DS-as-DDoS-amplifier**: A leaked layout (real, valid fh4) lets
   a third party hammer the DS port without going through MDS auth.
   Is mTLS at the DS the only mitigation, and does that hold? Is
   per-tenant DS rate-limiting required at the DS edge?
4. **TOCTOU between LAYOUTGET and shard split**: A composition
   layout issued at HLC=T₀ might reference shards that split at
   HLC=T₀ + ε. Can the I-PN5 1-sec recall miss a window where the
   client writes to a stripe whose shard membership has changed?
   What's the worst-case data loss / inconsistency?
5. **Cross-tenant via guessed composition_id**: composition_id is a
   UUID. fh4 binds tenant explicitly so MAC verification rejects
   forgeries, but is there any path where a legitimate tenant's fh4
   could be mutated and re-MAC'd to leak another tenant's data?
   (Belt-and-braces check on field ordering / canonicalization.)
6. **DS encryption boundary** (I-PN3): DS reads plaintext and writes
   plaintext on the wire (NFS protocol is plaintext). mTLS provides
   transport confidentiality. Is this correct under FIPS 140-3 boundary
   reasoning? Does the DEK ever cross the DS boundary? (Answer
   should be no — DEK stays in `kiseki-crypto`.)
7. **Op-subset escape** (I-PN7): the DS dispatcher allows only 8 op
   codes. Is there a known NFSv4.1 op that can be smuggled inside
   one of the 8 (e.g., COMPOUND embedding)? RFC 5661 §15.2 says
   COMPOUND can carry arbitrary ops — does the DS dispatcher run
   per-op or per-COMPOUND?
8. **LAYOUTRECALL non-delivery**: I-PN5 says recall is best-effort
   (TTL is the safety mechanism). Is 5-min stale routing acceptable
   in practice? Does it interact poorly with ADR-035 drain (a draining
   node may continue to serve I/O for up to 5 min after entering
   Drain)?
9. **Build-phase ordering**: Phase 15 sits after Phase 13 (cluster
   topology) but Phase 15c needs ADR-033/034/035 hooks. Are those
   hooks already exposed as observable events, or is new wiring
   required first? If new wiring, Phase 15c may need to split.

## Out of scope for this gate

- Implementation review (that comes after implementer step)
- Multi-cluster federation (ADR-022 territory)
- DS-side opens / loose-coupled FFL mode (explicitly deferred in ADR-038)

## Expected output

Findings dropped in `specs/findings/architecture-review.md` referencing
ADV-038-N tags. Block-list of must-fix items vs. nice-to-have items.
If structurally sound, a one-line "ADV-038 clear; implementer may
proceed to Phase 15a" suffices.
