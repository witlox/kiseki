# ADR-016: Backup and Disaster Recovery

**Status**: Accepted
**Date**: 2026-04-17
**Context**: A-ADV-8 (backup and DR)

## Decision

Federation is the primary DR mechanism. External backup is additive
and optional.

### Site-level DR via federation

- Federated-async replication to a secondary site is the primary DR story
- RPO: bounded by async replication lag (seconds to minutes)
- RTO: secondary site is warm (has replicated data + tenant config);
  switchover requires KMS connectivity and control plane reconfiguration
- Data replication is ciphertext-only (no key material in replication stream)

### What is replicated

| Component | Replicated? | Mechanism |
|---|---|---|
| Chunk data (ciphertext) | Yes | Async replication to peer site |
| Log deltas | Yes | Async replication of committed deltas |
| Control plane config | Yes | Federation config sync |
| Tenant KMS config | No | Same tenant KMS serves both sites |
| System master keys | No | Per-site system key manager |
| Audit log | Yes | Per-tenant audit shard replicated |

### External backup (optional, additive)

- Cluster admin can configure external backup targets (S3-compatible store)
- Backup contains: encrypted chunk data + log snapshots + control plane state
- Backup is encrypted with the system key (at rest) — no plaintext in backup
- HIPAA requirement met: backup is encrypted
- Backup frequency: configurable (hourly/daily snapshots of control plane,
  continuous for chunk data)

### Recovery scenarios

| Scenario | Recovery path | RPO | RTO |
|---|---|---|---|
| Single node loss | Raft re-election + EC repair | 0 | Seconds-minutes |
| Multiple node loss | Raft reconfiguration + EC repair | 0 | Minutes |
| Full site loss | Failover to federated peer | Replication lag | Minutes-hours |
| Site loss, no federation | Restore from external backup | Backup lag | Hours |
| Tenant KMS loss | Unrecoverable (I-K11) | N/A | N/A |

## Consequences

- Federation is the recommended (and primary) DR strategy
- External backup is for defense-in-depth, not primary recovery
- RTO for site failover depends on control plane reconfiguration speed
- System key manager is per-site — site failover requires the secondary site's
  own system key manager (different master keys, but tenants' data is
  accessible because tenant KMS is shared cross-site)
