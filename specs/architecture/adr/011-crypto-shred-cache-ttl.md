# ADR-011: Crypto-Shred Cache Invalidation and TTL

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-5 (crypto-shred propagation)

## Decision

Maximum tenant KEK cache TTL is **60 seconds**. Crypto-shred triggers
an **active invalidation broadcast** in addition to TTL expiry.

### Mechanism

1. Default cache TTL: 60 seconds (configurable per tenant, cannot exceed max)
2. On crypto-shred:
   a. KEK destroyed in tenant KMS
   b. Invalidation broadcast to all known gateways, stream processors,
      and native clients for that tenant
   c. Components receiving invalidation immediately purge cached KEK
   d. Components unreachable during broadcast will expire naturally at TTL
3. Crypto-shred operation returns success after KEK destruction + broadcast
   (does not wait for all acknowledgments)
4. Maximum residual window: 60 seconds (cache TTL for unreachable components)

### HIPAA/GDPR acceptability

- GDPR Art. 17 requires erasure "without undue delay" — 60 seconds is
  within reasonable interpretation for a distributed system
- HIPAA does not specify a time bound for deletion
- The audit log records exact times: KEK destroyed, broadcast sent,
  cache TTL expiry — providing compliance evidence

## Consequences

- 60-second maximum window where data is technically readable after shred
- Components must handle invalidation broadcast (new message type)
- Native clients on unreachable compute nodes: data readable until their
  process exits or TTL expires (whichever comes first)
- Shorter TTLs increase KMS load (more frequent key fetches)
