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

### TTL configuration (analyst backpass contention 3)

The 60-second TTL is the **default**, not a fixed value. TTL is
configurable per tenant within bounds:

| Parameter | Value | Rationale |
|---|---|---|
| Minimum TTL | 5 seconds | Below this, KMS load becomes problematic (key fetch every 5s per component) |
| Default TTL | 60 seconds | Reasonable for most deployments |
| Maximum TTL | 300 seconds (5 min) | Beyond this, the crypto-shred window is unreasonable |

Tenants under stricter regulation can request shorter TTL (e.g., 10s).
The trade-off is higher KMS load (more frequent key fetches). The control
plane validates that the requested TTL is within [min, max] and warns
if KMS capacity may be insufficient.

### HIPAA/GDPR acceptability

- GDPR Art. 17 requires erasure "without undue delay" — even 300 seconds
  is within reasonable interpretation for a distributed system
- HIPAA does not specify a time bound for deletion
- The audit log records exact times: KEK destroyed, broadcast sent,
  cache TTL expiry — providing compliance evidence
- Configurable TTL allows compliance-sensitive tenants to reduce the window

## Consequences

- Default 60-second window where data is technically readable after shred
- Configurable per tenant within [5s, 300s] bounds
- Components must handle invalidation broadcast (new message type)
- Native clients on unreachable compute nodes: data readable until their
  process exits or TTL expires (whichever comes first)
- Shorter TTLs increase KMS load (more frequent key fetches)
- TTL bounds are performance parameters that may conflict with compliance —
  the minimum (5s) is a hard engineering limit, not a policy choice
