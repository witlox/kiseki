# ADR-010: Retention Hold Enforcement Before Crypto-Shred

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-4 (retention hold ordering race)

## Decision

Compliance tags that imply retention requirements **automatically create
retention holds** when data is written. Crypto-shred checks for active
holds before proceeding.

### Mechanism

1. When a namespace has compliance tags (HIPAA, GDPR, etc.), the control
   plane derives retention requirements from the tag.
2. A **default retention hold** is automatically created for the namespace
   with the TTL mandated by the compliance regime.
3. Crypto-shred for a tenant checks all namespaces for active holds:
   - If holds exist: crypto-shred proceeds (KEK destroyed, data unreadable)
     but physical GC is blocked (correct behavior).
   - If no holds exist AND compliance tags imply retention: crypto-shred
     is **blocked** with an error requiring explicit override.
4. Override requires `force_without_hold_check: true` + audit log entry
   documenting the override and the reason.

### Compliance tag → retention mapping (configurable)

| Tag | Default retention | Source |
|---|---|---|
| HIPAA | 6 years | 45 CFR §164.530(j) |
| GDPR | Per DPA agreement | No fixed minimum |
| revFADP | Per data controller policy | Swiss FDPA Art. 6 |

## Consequences

- Retention holds are created automatically, reducing risk of human error
- Crypto-shred with override is audited (compliance team can review)
- Tenant admin can extend holds but not shorten below compliance minimum
