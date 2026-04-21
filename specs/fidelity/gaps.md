# Fidelity Gaps — 2026-04-21

## Domain Code Gaps (need new Rust modules)

| Gap | Module | Scenarios Blocked | Priority |
|-----|--------|-------------------|----------|
| NFS4 COMPOUND handlers | kiseki-gateway/nfs4_server | 27 | HIGH |
| NFS3 procedure handlers | kiseki-gateway/nfs3_server | 12 | HIGH |
| S3 HEAD/DELETE/LIST | kiseki-gateway/s3_server | 6 | HIGH |
| Client discovery protocol | kiseki-client/discovery | 20+ | MEDIUM |
| IdP/OIDC token validation | kiseki-control/idp | 3 | MEDIUM |
| SPIFFE SVID parsing | kiseki-transport/spiffe | 2 | MEDIUM |
| Crypto-shred through pipeline | kiseki-crypto/shred integration | 5 | HIGH |
| Key rotation worker (re-wrapping) | kiseki-keymanager/rotation | 4 | MEDIUM |
| Re-encryption engine | kiseki-keymanager/re_encrypt | 2 | LOW |
| Auto shard split | kiseki-log/auto_split | 3 | MEDIUM |
| Composition versioning | kiseki-view/versioning | 4 | MEDIUM |
| View staleness SLO enforcement | kiseki-view | 3 | MEDIUM |
| Compress-then-encrypt pipeline | kiseki-crypto/compression | 3 | LOW |
| Format version negotiation | kiseki-common/versioning | 3 | LOW |
| ptrace/integrity monitor | kiseki-server/integrity | 2 | LOW |
| Admin gRPC (StorageAdminService) | kiseki-control/grpc | 30 | MEDIUM |
| Client transport selection | kiseki-client/transport_select | 4 | LOW |
| Client batching/prefetch | kiseki-client | 4 | LOW |
| NFS4 lock state machine | kiseki-gateway/nfs4_server | 3 | LOW |
| S3 conditional writes | kiseki-gateway/s3_server | 2 | LOW |

## Infrastructure Gaps (need multi-process/distributed testing)

| Gap | Scenarios | Notes |
|-----|-----------|-------|
| Multi-node Raft harness | 18 | Need 3 in-process Raft groups |
| Persistence BDD setup | 12 | Background step for redb |
| Gateway crash/recovery | 3 | Need process restart |
| Rolling upgrade | 3 | Mixed-version cluster |
| Federation cross-cluster | 3 | External cluster |

## Resolved This Session

- ~~Go control plane~~ → ADR-027 Rust kiseki-control (32/32)
- ~~EC erasure coding~~ → ec.rs + placement.rs (14/14)
- ~~Device management~~ → device.rs + thresholds (19/19)
- ~~Gateway pipeline~~ → InMemoryGateway in BDD (8/21)
- ~~BDD honesty~~ → 1125 empty stubs → panics (205/456 honest)
