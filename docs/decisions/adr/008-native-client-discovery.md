# ADR-008: Native Client Fabric Discovery

**Status**: Accepted
**Date**: 2026-04-17
**Context**: Escalation point 8, A-ADV-1, I-O4

## Decision

Native clients discover shards, views, and gateways via a lightweight
**discovery service** running on every storage node, accessible on the
data fabric. No control plane access required.

## Mechanism

1. **Bootstrap**: client is configured with a list of seed endpoints
   (storage node addresses on the data fabric). Seed list can be
   provided via environment variable, config file, or DHCP option.

2. **Discovery query**: client sends a discovery request to any seed.
   The storage node responds with:
   - List of active shards (shard_id, leader node, key range)
   - List of materialized views (view_id, protocol, endpoint)
   - List of gateway endpoints (protocol, transport)
   - Tenant authentication requirements

3. **Authentication**: client presents mTLS certificate (Cluster CA signed,
   per-tenant). Optional second-stage auth via tenant IdP.

4. **Cache**: discovery results cached with TTL. Periodic refresh.
   Shard split/merge events invalidate relevant cache entries.

5. **Transport negotiation**: client probes available transports
   (CXI → verbs → TCP) and selects highest-performance option.

## Why not DNS-SD or multicast

- Slingshot fabric may not support multicast reliably
- DNS-SD requires DNS infrastructure on the data fabric
- Seed-based discovery is simple, deterministic, and works with any transport

## Consequences

- Every storage node runs a discovery responder (lightweight, part of kiseki-server)
- Seed list is the only bootstrap configuration for compute nodes
- Discovery responder must not expose tenant-sensitive information
  (shard/view metadata is operational, not tenant content)
