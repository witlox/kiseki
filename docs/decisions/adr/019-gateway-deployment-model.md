# ADR-019: Gateway Deployment Model

**Status**: Accepted
**Date**: 2026-04-17
**Context**: ADV-ARCH-03 (monolith blast radius), analyst backpass contention 4

## Decision

Gateways run **in-process with kiseki-server** (monolith per node). Client
resilience is provided by multi-endpoint resolution, not per-process
gateway isolation.

### Rationale

This is a distributed system with no master. Every storage node runs
kiseki-server (log + chunk + composition + view + gateways). Clients
resolve to multiple endpoints:

```
Client (NFS/S3/native)
  │
  ├── DNS round-robin: kiseki-nfs.cluster.local → [node1, node2, node3, ...]
  ├── Multiple A/AAAA records
  ├── Native client: seed list → discovery → multiple endpoints
  │
  └── On node failure: client reconnects to next endpoint
      (NFS: automatic reconnect; S3: retry to different host;
       native: transport failover)
```

### Why monolith is acceptable

| Concern | Mitigation |
|---|---|
| Gateway crash = node crash | Client reconnects to another node (seconds) |
| All tenants on crashed node affected | Tenants are served by multiple nodes; one node loss = partial, not total |
| Memory leak in gateway affects log/chunk | Resource limits via cgroups; OOM killer targets the process, not the node |
| Bug in NFS gateway affects S3 gateway | Accept — both are in the same process. Isolation adds operational complexity disproportionate to the risk |

### Why NOT separate gateway processes

- Additional process management per node (spawn, monitor, restart, IPC)
- Performance overhead of IPC between gateway and log/chunk/view
- Operational complexity (more processes to configure, monitor, upgrade)
- The resilience model is **client-side multi-endpoint**, not **server-side process isolation**

### Client resolution

| Client type | Resolution mechanism |
|---|---|
| NFS | DNS (multiple A records), NFS mount with multiple server addresses |
| S3 | DNS round-robin, HTTP retry to next endpoint on 5xx |
| Native | Seed list → fabric discovery → multiple endpoints, automatic failover |

## Consequences

- kiseki-server remains a single-process monolith per node
- Client-side resilience is the primary availability mechanism
- Update failure-modes.md: F-D1 (gateway crash) → node-scoped, not protocol-scoped
- Node loss tolerance depends on tenant data distribution across nodes
