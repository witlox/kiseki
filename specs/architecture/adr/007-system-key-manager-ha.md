# ADR-007: System Key Manager HA via Raft

**Status**: Accepted
**Date**: 2026-04-17
**Context**: I-K12, escalation point 7, B-ADV-3

## Decision

The system key manager is a dedicated Raft group (3 or 5 members) running
as `kiseki-keyserver` on dedicated nodes. It stores system master keys
(one per epoch) and derives per-chunk DEKs via HKDF at runtime (ADR-003).

## Architecture

```
kiseki-keyserver (3-5 nodes, Raft)
  ├── Stores: system master keys (one per epoch, ~32 bytes each)
  ├── Derives: system DEK = HKDF(master_key, chunk_id) — stateless
  ├── Manages: epoch lifecycle (create, rotate, retain, destroy)
  └── Audits: all key events to audit log
```

## Rationale

- System key manager is the highest-severity SPOF (P0 if unavailable)
- Must be at least as available as the Log
- Raft provides consensus + replication + leader election
- Separate from shard Raft groups (independent failure domain)
- Dedicated nodes: key material never co-located with tenant data
- Master key storage is trivial (epochs × 32 bytes)
- DEK derivation is stateless and fast (HKDF, ~microseconds)

## Deployment

- 3 nodes for standard deployments, 5 for high-criticality
- Dedicated hardware (or at minimum, dedicated processes on control-plane nodes)
- Key material in memory only (mlock'd, guard pages)
- On-disk: Raft log + snapshot of epoch state (encrypted with node-local key)

## Consequences

- Adds a deployment component (`kiseki-keyserver`)
- Key manager must be deployed and healthy before any data operations
- Cross-site: each site has its own system key manager (federation doesn't
  share system keys — only tenant keys cross sites via tenant KMS)
