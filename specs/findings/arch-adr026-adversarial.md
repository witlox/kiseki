# Architecture Adversarial Review: ADR-026 (Raft Topology) + Implementation

**Date**: 2026-04-20. **Reviewer**: Adversary (architecture mode).

## CRITICAL (5)

### RAFT-C1: Small writes <4KB go entirely through Raft (not just metadata)
- LogCommand::AppendDelta includes payload Vec<u8> up to 4KB (inline threshold)
- Metadata-heavy workloads: 50-100k ops/sec × 2KB = 100-200 MB/s through Raft
- Performance projections assume "1KB metadata" — invalid for small-file workloads

### RAFT-C2: Groups-per-node not guaranteed at ~30 under placement constraints
- Rack-aware placement, tenant affinity pools, and evacuation create hotspots
- Some nodes may host 50-60+ groups temporarily

### RAFT-C3: Election storm recovery time not quantified
- TiKV sees 30-60s recovery at scale, not "seconds"
- Needs simulation or real testing

### RAFT-C4: StubNetworkFactory blocks multi-node — no TCP transport exists
- All three Raft groups use Unreachable stubs
- TCP+TLS transport must be implemented for multi-node

### RAFT-C5: MemLogStore not persistent; RedbLogStore unvalidated multi-node
- Current code uses MemLogStore (data lost on restart)
- RedbLogStore exists but never tested with concurrent Raft peers

## HIGH (5)

### RAFT-H1: TLS overhead on heartbeats not modeled
### RAFT-H2: Staggered group startup not implemented
### RAFT-H3: Key material snapshots unencrypted in multi-node
### RAFT-H4: Dual sync/async LogOps paths risk divergence
### RAFT-H5: Zero multi-node tests exist

## MEDIUM (5)

### RAFT-M1: Write size distribution not validated against perf model
### RAFT-M2: TCP connection pooling design missing
### RAFT-M3: Multi-node persistent storage strategy undefined
### RAFT-M4: Snapshot install could lose audit events
### RAFT-M5: Heartbeat timeout (500ms) aggressive for lossy TCP

## LOW (3)

### RAFT-L1: Jitter-based de-correlation insufficient for large clusters
### RAFT-L2: Placement constraint not enforced
### RAFT-L3: Management network routing not documented
