# ADR-012: Stream Processor Tenant Isolation

**Status**: Accepted
**Date**: 2026-04-17
**Context**: B-ADV-6 (stream processor isolation)

## Decision

Stream processors for different tenants run in **separate OS processes**
on storage nodes. Key material is protected with `mlock` and guard pages.

### Isolation model

| Mechanism | Purpose |
|---|---|
| Separate processes | OS-level memory isolation between tenants |
| mlock on key pages | Prevent key material from swapping to disk |
| Guard pages | Detect buffer overflows near key material |
| seccomp (Linux) | Restrict syscalls to minimum needed |
| Separate cgroups | Resource isolation (CPU, memory) per tenant |

### Co-location policy

- Small tenants: multiple stream processors per node (process isolation)
- Large/sensitive tenants: dedicated nodes (configurable via placement policy)
- Compliance tags can mandate dedicated nodes (e.g., HIPAA with strict isolation)

### Hardware isolation (future)

- AMD SEV-SNP / Intel TDX confidential VMs: out of scope for initial build
- Envelope format and key wrapping are compatible with confidential compute
  (keys are already protected end-to-end; adding a TEE is additive, not
  architectural change)

## Consequences

- More processes per storage node (one per tenant per view)
- Process management in kiseki-server (spawn, monitor, restart)
- Memory overhead per process (Rust process ~10-20MB base)
- Key material never in shared memory between tenants
