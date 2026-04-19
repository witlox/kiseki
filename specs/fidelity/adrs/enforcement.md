# ADR Enforcement Assessment

| ADR | Decision (1-line) | Enforcement | Rating |
|-----|-------------------|-------------|--------|
| 001 | Pure Rust, no Mochi dependency | All data-path crates are Rust | ENFORCED |
| 002 | Two-layer encryption model C | `kiseki-crypto` envelope: system DEK + tenant KEK wrap | ENFORCED |
| 003 | System DEK derivation via HKDF locally | `hkdf.rs` derives locally, key manager never sees chunk IDs | ENFORCED |
| 004 | Schema versioning and upgrade | No versioning tests | UNENFORCED |
| 005 | EC and chunk durability | `DurabilityStrategy` enum exists, no EC impl | DOCUMENTED |
| 006 | Inline data threshold | `has_inline_data` field on `DeltaHeader`, tested | ENFORCED |
| 007 | System key manager HA (Raft) | No Raft, `MemKeyStore` only | DOCUMENTED |
| 008 | Native client discovery | Discovery types exist, no actual protocol | DOCUMENTED |
| 009 | Audit log sharding | Per-tenant `ShardKey` routing in `AuditLog` | ENFORCED |
| 010 | Retention hold enforcement | `retention_hold_blocks_gc` test | ENFORCED |
| 011 | Crypto-shred cache TTL | No cache TTL implementation | UNENFORCED |
| 012 | Stream processor isolation | Per-tenant design in view, no process isolation | DOCUMENTED |
| 013 | POSIX semantics scope | `ProtocolSemantics::Posix` enum, no NFS impl | DOCUMENTED |
| 014 | S3 API scope | `ProtocolSemantics::S3` enum, no S3 impl | DOCUMENTED |
| 015 | Observability | No metrics/tracing | UNENFORCED |
| 016 | Backup and DR | No backup implementation | UNENFORCED |
| 017 | Dedup refcount access control | Chunk ID derivation per dedup policy tested | ENFORCED |
| 018 | Runtime integrity monitor | Not implemented | UNENFORCED |
| 019 | Gateway deployment model | Single gateway crate with feature flags | DOCUMENTED |
| 020 | Workflow advisory (analyst) | Advisory types in `kiseki-common`, feature file exists | DOCUMENTED |
| 021 | Advisory architecture | Budget enforcer, workflow table, lookup impl | ENFORCED |

## Summary

- **ENFORCED**: 8/21 (38%) — test fails if violated
- **DOCUMENTED**: 8/21 (38%) — types/code exist but no enforcement test
- **UNENFORCED**: 5/21 (24%) — no code or tests
