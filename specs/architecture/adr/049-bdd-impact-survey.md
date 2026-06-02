# ADR-049 BDD impact survey

User-requested companion to the ADR-049 implementation work. Lists every
existing `specs/features/*.feature` that touches the affected paths (boot
sequence, `data_dir` paths, the four catalog-resolved fjall stores,
`SmallObjectStore`, control-plane state machine) and classifies each
scenario as `BREAKS` (must adapt before phase 5 merge), `NEEDS-ADAPTATION`
(passes today but extending the scenario adds value), or `UNCHANGED`.

The new scenarios DI-1..DI-7 live in `specs/features/device-inventory.feature`.

## Existing BDDs that BREAK after phase 5

These reference `small/objects.redb` or hardcoded `data_dir` paths that
no longer exist post-fjall-swap (phase 5b) and post-boot-reorder (phase 5a).

### `specs/features/small-file-placement.feature`

| Scenario | Lines | Breakage | Adaptation |
|---|---|---|---|
| "File below threshold stored inline via Raft" | 28-34 | "on apply the state machine offloads the payload to `small/objects.redb`" — redb no longer exists | Replace `small/objects.redb` with `<resolved SmallObject path>/objects` (resolver output); keep the I-SF5 inline-offload semantic |
| "Inline read hits small store" | 49-54 | `ChunkOps::get() finds it in small/objects.redb` | Same path substitution; `ChunkOps::get` consults the resolver-routed fjall keyspace |
| "Snapshot includes inline content (I-SF5)" | 59-67 | Snapshot mentions `small/objects.redb` build/restore | Substitute fjall keyspace path; keep the round-trip semantic |
| "Inline file deletion cleans `small/objects.redb`" | 82-88 | Same path | Substitute; semantic stays |
| "Orphan detection in `small/objects.redb`" | 92-99 | Same path | Substitute; semantic stays |
| Multiple scenarios mention `small/objects.redb is populated via log replay` | 149-160 | Path | Substitute |

**Action:** scripted find-and-replace `small/objects.redb` →
`<resolved SmallObject path>/objects` across this feature file as part of
phase 5b. Where the scenario specifically asserted "redb transactions"
(none today), replace with "fjall batch commit + persist mode".

### `specs/features/persistence.feature`

| Scenario | Lines | Breakage | Adaptation |
|---|---|---|---|
| Four references to `small/objects.redb` | grep -n above | Hardcoded path; redb removed | Substitute fjall path post-phase-5b |

**Action:** same substitution.

## Existing BDDs that NEED-ADAPTATION

These pass on phase 5 as-is but would gain coverage by extending them to
cover the new behavior. Not blocking.

### `specs/features/control-plane.feature`

Scenarios that submit `CreateNamespace` / `RecordSplit` and assert apply
side-effects — none of these touch the catalog state, but adding a
"control-plane state machine snapshot includes the catalog" assertion to
existing snapshot tests would catch regressions.

| Scenario | Add | Reason |
|---|---|---|
| "Control plane unavailable — data path continues" | "and the resolver uses the locally-cached `kiseki-tier-paths.json` pointer; opened fjall stores remain open at their cached paths" | Tests rev-4 Risk-section policy-cache fallback |

### `specs/features/cluster-formation.feature`

Boot-ordering scenarios for the existing seed-node + follower flow.

| Scenario | Add | Reason |
|---|---|---|
| "Seed node initializes and becomes leader" (line 13) | And-clause: "the seed publishes its `UpsertNodeInventory` before opening catalog-resolved fjall stores" | Documents the §D2.5 RaftLog bootstrap-only + N-9 publish-before-resolver-gate ordering |
| "Follower joins existing cluster" (line 28) | And-clause: "the joining node receives the catalog via the control-plane snapshot install path" | Documents Q36 `#[serde(default)]` forward-compat |
| "Namespace creation produces 3x node_count shards" (line 106) | And-clause: "the catalog reflects each node's inventory before the shards' Raft groups are created" | Documents the boot-order interplay between catalog publish and per-shard create |

### `specs/features/multi-node-raft.feature`

Crash-recovery scenarios.

| Scenario | Add | Reason |
|---|---|---|
| "Crashed node recovers from local log + network" (line 117) | And-clause: "on recovery the node publishes its current `UpsertNodeInventory` BEFORE the resolver runs (N-9: a recovering node with stale catalog truth must self-correct before the resolver gate)" | Documents N-9 acceptance |

### `specs/features/storage-admin.feature`

Per-pool admin commands.

| Scenario | Add | Reason |
|---|---|---|
| Pool device-class scenarios | Cross-reference: "and ADR-049 `MediaType` (catalog) maps to ADR-024 `DeviceClass` via §D11.1 Q13 mapping; QLC inferred from `DeviceClass::NvmeQlc` emits `kiseki_placement_qlc_inferred{node, mount} = 1`" | Documents the ADR-024 / ADR-049 axis disambiguation |

## Existing BDDs that are UNCHANGED

These remain orthogonal to ADR-049 (the dependency-firewalled crates
they cover don't see the catalog):

- `authentication.feature` — IAM only; no fjall consumers
- `backup-and-restore.feature` — ADR-016 already includes the
  control-plane state-machine snapshot (now includes the catalog
  field; serialization is JSON so no wire-format change)
- `block-storage.feature` — raw block / `KISEKI_RAW_DEVICES` axis
  stays orthogonal per §D11.1
- `chunk-storage.feature` — chunk meta moves under ADR-049 but the
  semantic invariants (cluster_chunk_state etc.) don't change
- `composition.feature` — CompositionStore moves under ADR-049 but
  ADR-040 invariants stay (I-CP1 now guarded by I-CP-Move on path
  moves)
- `erasure-coding.feature` — EC fragments stay on raw block devices
- `external-kms.feature` — KMS surface; orthogonal
- `key-management.feature` — KEK rotation; orthogonal
- `log.feature` — non-catalog log scenarios
- `multi-node-nfs.feature` — NFS service; orthogonal to the resolver
- `native-client.feature` — client wire; orthogonal
- `native-gateway.feature` — gateway API; orthogonal
- `nfs3-rfc1813.feature` / `nfs4-rfc7862.feature` / `pnfs-rfc8435.feature` — NFS protocol surfaces
- `operational.feature` — generic operational scenarios; orthogonal
- `protocol-gateway.feature` — protocol shape
- `s3-api.feature` — S3 protocol
- `server-harness-smoke.feature` — harness-only
- `view-materialization.feature` — view jobs
- `workflow-advisory.feature` — advisory surface
- `device-management.feature` — pre-existing pool admin

## Implementation order for the BDD work

Phase 6 of ADR-049 handles BDD landing. Sequence within phase 6:

1. Land `specs/features/device-inventory.feature` with DI-1..DI-7.
2. Adapt `small-file-placement.feature` + `persistence.feature` —
   substitute every `small/objects.redb` with the resolver-output
   path; preserve the invariant assertions verbatim.
3. Extend `cluster-formation.feature` boot scenarios with the
   §D2.5 RaftLog + N-9 publish-before-resolver ordering clauses.
4. Add the I-CP-Move recovery clause to
   `multi-node-raft.feature::"Crashed node recovers from local log + network"`.
5. Optionally extend `control-plane.feature::"Control plane unavailable"`
   with the policy-cache fallback clause (depends on whether
   rev-4 Risk-section caching ships in phase 5 — if deferred,
   skip this scenario for now).
6. Cross-reference `storage-admin.feature` pool scenarios with the
   ADR-024 / ADR-049 MediaType mapping if the implementer has
   landed the Q13 mapping table in code.

## DI-1..DI-7 acceptance criteria (cross-reference)

| Scenario | Covers | ADR-049 reference |
|---|---|---|
| DI-1 | Single-NVMe-node default policy → SmallObject on NVMe | §D4 default policy table |
| DI-2 | §D4.5 worked-example arithmetic (heterogeneous F=7700 GiB) | §D4.5 + rev 4 §revision-history MUST-FIX 3 |
| DI-3 | Strict mode missing-device-class refusal | §D4 PolicyMode::Strict + Q22 N-12 ordering |
| DI-4 | Placement-policy change + operator migration (placement-only) | §D8 migration v1 |
| DI-4b | I-CP-Move enforces refuse-to-open on non-quiesced reboot | §D8.1 + I-CP-Move + Q31 N-11 |
| DI-5 | I-DI8 / I-DI9 Absolute overcommit rejected at apply | §D9 I-DI9 + §D4.5 cluster-aggregate pre-check |
| DI-6 | Pointer-file deleted out of band → RefuseToOpen | §D8.1 + Q23 N-2 |
| DI-7 | await_catalog_ready quiescence semantics under inventory churn | §D5.5 + rev-4 N-5 |

## Tagging convention

- `@adr-049` on every new scenario for easy filter (`-t @adr-049`).
- `@device-inventory` for catalog / inventory scenarios.
- `@capacity` for formula scenarios.
- `@strict-mode` for Strict policy refusals.
- `@migration` for migration scenarios.
- `@cp-move` for I-CP-Move scenarios.
- `@pointer-file` for `kiseki-tier-paths.json` scenarios.
- `@await-catalog` for the boot-readiness gate.
- `@flaky` on DI-7 only (timing-sensitive); pair with `cucumber retry_filter`.
