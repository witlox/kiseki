# Architecture Adversarial Review: ADR-024 (Device Management)

**Date**: 2026-04-20. **Reviewer**: Adversary (architecture mode).

Note: ADR-024 was updated mid-review to add per-device-class thresholds
and eviction policy. Findings reflect the post-update state.

## CRITICAL (4)

### ADR024-C1: HDD/SSD device classes missing from code enum
- **Location**: data-models/chunk.rs has NvmeU2/NvmeQlc/NvmePersistentMemory/Custom
- **Missing**: SsdSata, HddEnterprise, HddBulk from ADR-024
- **Resolution**: Add to DeviceClass enum in chunk.rs

### ADR024-C2: No capacity threshold invariants in invariants.md
- **Location**: invariants.md has no I-C5 for capacity enforcement
- **Missing**: "Pool writes rejected at Critical", "ENOSPC at Full"
- **Resolution**: Add invariants, update enforcement-map.md

### ADR024-C3: Device evacuation trigger semantics still vague
- **Issue**: ADR-024 now has evacuation table but doesn't specify:
  automatic vs admin-only for each trigger, evacuation SLO,
  in-flight write handling during evacuation
- **Status**: Partially addressed by update (triggers listed), 
  needs SLO and in-flight write semantics

### ADR024-C4: System partition RAID-1 responsibility unspecified
- **Issue**: Is OS or Kiseki responsible? What if RAID degrades?
- **Resolution**: Document as OS-managed, add failure mode for 
  system partition degradation. Kiseki should monitor /proc/mdstat.

## HIGH (4)

### ADR024-H1: Per-device-class thresholds ✓ RESOLVED
- Was: global 80/90/95/99% for all devices
- Now: SSD 75/85/92/97%, HDD 85/92/97/99% (in updated ADR)

### ADR024-H2: Pool redirection on Critical unspecified
- What is a "sibling pool"? How discovered? Cross-compliance risk?
- **Resolution**: Define sibling as same-tier pool on other devices

### ADR024-H3: No invariant for auto-repair on device failure
- Missing I-D1: "chunks on failed device auto-repaired from EC"
- **Resolution**: Add invariant, define repair trigger component

### ADR024-H4: Device state transition audit trail missing
- No audit events for Healthy→Degraded, Evacuating→Removed
- **Resolution**: Define audit event types for device lifecycle

## MEDIUM (5)

### ADR024-M1: Filesystem/discovery assumptions unspecified
- ✓ PARTIALLY RESOLVED: xfs default + manual config documented
- Still missing: device rename handling, hot-plug

### ADR024-M2: ADR-005/024 pool durability alignment
- Can pools change composition? EC rebalance on device add?

### ADR024-M3: Failure mode blast radius incomplete for pools
- Pool state during repair undefined (ReadOnly? Normal?)

### ADR024-M4: Reactive tiering has no correctness model
- A8 marked Unknown but ADR treats as decided

### ADR024-M5: Pool redirection could violate dedup/affinity (I-X2, I-C3)

## LOW (4)

### ADR024-L1: Latency baseline for Degraded undefined
### ADR024-L2: Bad sector threshold (>100) is ad-hoc
### ADR024-L3: Wear level 90% may be conservative for enterprise SSDs
### ADR024-L4: PoolHealth threshold boundary semantics unclear
