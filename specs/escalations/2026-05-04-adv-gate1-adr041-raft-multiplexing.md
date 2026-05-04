# Adversary Gate 1 Request — ADR-041 (Raft transport shard multiplexing)

**Type**: Architect → Adversary
**Date**: 2026-05-04
**Author**: architect
**Status**: closed 2026-05-04 — gate 1 returned CHANGES REQUESTED; architect amended ADR-041 (commit pending). 3H + 5M + 2L findings addressed inline in the ADR; remaining 5L are non-blocking notes. Re-review by adversary deferred — implementer can proceed under the revised spec.

## Artifacts to review

1. `specs/architecture/adr/041-raft-transport-shard-multiplexing.md`
2. Existing `crates/kiseki-raft/src/tcp_transport.rs` (the code being
   refactored)
3. ADR-026 §"Transport" (phase mapping amended by ADR-041)

## Why this needs gate 1

ADR-041 is the unblocker for ADR-033 / ADR-034 multi-shard topology.
It introduces:

- A new wire format prefix (version byte + shard_id) that breaks
  compatibility with pre-ADR-041 nodes — flag-day cutover.
- A per-node listener that owns a registry of typed `Raft<C, SM>`
  handles via type-erased closure dispatchers — lifetime + lock
  semantics under concurrent membership change need scrutiny.
- An empty-response convention for `unknown_shard` that callers
  must treat as transient — a stale shard cache could degrade into
  silent infinite retry loops if the caller's policy is wrong.
- mTLS handshake on a single port for traffic from N peer Raft
  groups — peer-cert-binding semantics shift from per-shard to
  per-node-pair.

## Specific questions for adversary

1. **Wire-format DoS**. The version byte is 1 byte; the shard_id is
   a 36-byte ASCII UUID inside the JSON tuple; the rest is the
   openraft request. Can a malicious peer with a valid cert burn
   server CPU by sending high-rate frames with valid version + valid
   shard_id but malformed payload that survives the version check
   but fails JSON deserialization for *that* shard's `C::D`? The
   current per-frame `tokio::spawn` pattern means each malformed
   frame still allocates a task. Bound?

2. **Stale shard cache after merge**. ADR-034 retires a shard after
   a 5-minute grace period. A peer with a stale `NamespaceShardMap`
   continues to send RPCs to the retired `shard_id`. The listener
   responds with empty (treated as transient). The caller's openraft
   `RaftNetwork::append_entries` interprets the empty response as
   what — a network failure, triggering retry forever? Verify the
   caller path doesn't hot-loop.

3. **Registry generation race**. A shard is unregistered (line
   `listener.unregister_shard(id)`) then re-registered with a new
   `Raft<C, SM>` for a membership-change scenario. An RPC frame
   that arrived between unregister and re-register sees a different
   dispatcher than expected. Is there a TOCTOU where the RPC's vote
   ends up applied to the wrong epoch's state machine? Need to
   confirm openraft's vote/term semantics make this impossible
   regardless of dispatcher swap.

4. **DashMap contention under split storm**. ADR-033 §"Rate limiting
   (ADV-033-7)" caps concurrent splits at `max(1,
   active_node_count / 5)`. Even at the cap, a 50-node cluster could
   register 10 new shards in a burst. DashMap shards by hash; could
   a pathological hash distribution serialize all registrations on
   one shard's lock? (Probably not at this scale, but worth a
   sentinel.)

5. **TLS renegotiation cost**. The single-port multiplexing means
   one TLS session amortizes across N shards' RPC traffic to a
   peer. If the peer's cert is rotated mid-flight (ADR-007 cert
   epoch change), all shards' RPCs to that peer wait on the
   handshake. Acceptable, or should rotations be coordinated with
   listener restart?

6. **`MAX_RAFT_RPC_SIZE` interaction**. The shard_id prefix adds
   ~50 bytes overhead per frame. Snapshot transfers that previously
   fit just under the cap could now exceed it after the prefix is
   added. Audit: does the existing snapshot path size-pad against
   `MAX_RAFT_RPC_SIZE` exactly, or with headroom? If exact, the
   prefix tips it over.

7. **Listener startup race**. The runtime spawns the listener task
   asynchronously. A peer that connects before the listener has
   bound gets `ECONNREFUSED`; openraft retries with backoff. But a
   peer that connects *after* bind but *before* the first
   `register_shard` call sees `unknown_shard` for every RPC. Should
   the listener buffer pre-registration RPCs, or is the openraft
   retry sufficient?

## What's NOT in scope for this gate

- Implementation review — that's gate 2 (auditor) post-implementation.
- Performance benchmarks — those run after the implementation lands.
- Heartbeat batching (ADR-026 Strategy C proper) — a follow-on
  optimization on top of the multiplexed transport, not this ADR.

## Approval criteria

Adversary returns one of:
- **APPROVED** — implementer may proceed. Gate findings (if any)
  are addressed in the implementation phase per the standard
  RED-then-GREEN cycle.
- **CHANGES REQUESTED** — specific issues that the architect must
  address in the ADR before implementation starts.
- **BLOCKED** — fundamental design issue requiring ADR rework or
  analyst escalation (e.g., a domain assumption baked into ADR-041
  is wrong).
