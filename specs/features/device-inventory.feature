Feature: ADR-049 cluster-side device inventory + per-tier metadata placement
  ADR-049 lets fjall stores route to the right device on heterogeneous-hardware
  clusters via: per-node device inventory (D1) + cluster catalog (D2 — control-
  plane Raft state machine) + placement policy (D4) + capacity formula (D4.5) +
  per-node resolver (D5). Phase 6 of the implementation lands these scenarios.

  Background:
    Given a cluster running the kiseki-server build with ADR-049 phases 1-5

  @adr-049 @device-inventory
  Scenario: DI-1 — single-NVMe-node cluster routes SmallObject to NVMe (default policy)
    Given a 1-node cluster with `/mnt/nvme0` mounted on a NVMe-class device
    And the operator has NOT customized the placement policy
    When the node boots through `runtime::start`
    Then the resolver picks `MediaType::Nvme` for the `SmallObject` tier
    And the SmallObject fjall store opens at `/mnt/nvme0/kiseki/small-object`
    And the same path resolution holds for `IntentStore`, `CompositionMeta`, `ChunkMeta`
    And the `kiseki-tier-paths.json` pointer file records the four resolved paths

  @adr-049 @device-inventory @capacity
  Scenario: DI-2 — heterogeneous 6-node cluster matches the §D4.5 worked-example arithmetic
    Given a 6-node cluster where
      | node | fast (GiB NVMe) | slow (GiB SATA) |
      | 1    | 1500            | 8000            |
      | 2    | 1500            | 8000            |
      | 3    | 1500            | 8000            |
      | 4    | 200             | 0               |
      | 5    | 1500            | 8000            |
      | 6    | 1500            | 8000            |
    And the default `WorkloadParams` are in effect (avg_file_bytes=256 KiB, R=3, growth=1.5, fast_headroom_pct=25, metadata_ceiling_pct_of_fast=30)
    When `compute_cluster_budgets` runs against the catalog
    Then `f_total` equals 7700 GiB
    And node 4's share equals 200/7700 = 2.5974%
    And the per-node sum (metadata + small_object + headroom) on node 4 equals exactly 200 GiB
    And the per-node sum on each lssd node equals exactly 1500 GiB
    And I-DI8 (per-node budget sum ≤ node.fast_capacity) holds for every node

  @adr-049 @strict-mode
  Scenario: DI-3 — Strict-mode policy with missing device class refuses to start
    Given a 1-node cluster whose only filesystem device is SATA SSD (no NVMe)
    And the operator has set `PlacementPolicy.tiers[SmallObject]` with
      | preferences | [Class(Nvme)]                  |
      | mode        | Strict                          |
    When the node boots through `runtime::start`
    Then the resolver returns `PlacementError::NoMatchingDevice { tier: SmallObject, mode: Strict, ... }`
    And the node exits with status 1
    And the structured error log line includes the policy preferences and the inventory device list

  @adr-049 @migration
  Scenario: DI-4 — placement-policy change + operator-driven migration (placement-only case)
    Given a 3-node cluster where SmallObject currently lives on `/mnt/sata0` (SSD)
    When the operator submits `SetPlacementPolicy` setting SmallObject preferences to `[Class(Nvme), Class(Ssd)]`
    Then `policy_revision` bumps by 1 and `policy_change_ms` advances
    And each node's current SmallObject `chosen_mount` still points to `/mnt/sata0` (boot-time memoization)
    And `kiseki-admin topology node-inventory show` reports a `placement_path_mismatch` for each node
    When the operator runs `kiseki-admin storage migrate --tier=SmallObject --node=1`
    Then node 1 quiesces SmallObject writes, copies the keyspace `/mnt/sata0/kiseki/small-object → /mnt/nvme0/kiseki/small-object`, updates `kiseki-tier-paths.json` atomically, and clears the quiesce
    And node 1's `placement_path_mismatch` gauge returns to 0
    And nodes 2 + 3 continue to serve from `/mnt/sata0` until migrated

  @adr-049 @migration @cp-move
  Scenario: DI-4b — non-quiesced reboot during pending migration refuses to start (I-CP-Move)
    Given a 3-node cluster where the operator just changed SmallObject placement (DI-4)
    And node 2 has NOT yet been migrated
    When node 2 is restarted (e.g. for OS maintenance) WITHOUT running `storage migrate`
    Then on boot node 2's resolver tries to open SmallObject at the new path `/mnt/nvme0/kiseki/small-object`
    And the `kiseki-tier-paths.json` pointer reads the prior path `/mnt/sata0/kiseki/small-object`
    And a non-empty fjall keyspace still exists at the prior path
    Then I-CP-Move trips: node 2 refuses to open SmallObject and exits with `PathVersionMismatch`
    And the structured log line names the prior path, the resolved path, and the `kiseki-admin storage migrate` command the operator must run
    And nodes 1 + 3 continue serving from their respective (already-migrated or not-yet-migrated) paths

  @adr-049 @capacity @policy-overcommit
  Scenario: DI-5 — Absolute SmallObject overcommit rejected at SetPlacementPolicy apply (I-DI9)
    Given a 6-node cluster with cluster-wide `F_total` = 9 TiB
    When the operator submits `SetPlacementPolicy` with `SmallObject.capacity = Absolute { cluster_bytes: 100 TiB }`
    Then the cluster-aggregate Absolute pre-check fires before per-node distribution
    And `SetPlacementPolicy` returns `Err(PolicyOvercommit { cluster_demand: 100 TiB, cluster_available: ~6.75 TiB })`
    And the catalog state machine REJECTS the LogCommand (policy_revision is NOT bumped, policy stays unchanged)
    And the admin CLI surfaces the error verbatim so the operator sees the demand vs available numbers

  @adr-049 @cp-move @pointer-file
  Scenario: DI-6 — pointer-file deleted out of band refuses to start
    Given a 1-node cluster where `kiseki-tier-paths.json` records all four tier paths
    When the operator deletes `kiseki-tier-paths.json` out of band (e.g. an over-eager filesystem cleanup script)
    And the node is restarted
    Then on boot the resolver finds the four catalog-resolved paths
    And the pointer file is missing
    Then the boot path treats "missing pointer" as `RefuseToOpen` (NOT first-boot) because non-empty fjall keyspaces exist at the resolved paths
    And the node refuses to start with a clear error pointing to `kiseki-admin topology node-inventory show`
    # Acceptance criterion N-2: corrupt JSON case behaves identically.

  @adr-049 @await-catalog @flaky
  # @flaky: depends on tokio-rt timing for the quiescence clock; pair with retry_filter
  Scenario: DI-7 — await_catalog_ready quiescence-timeout interplay (rev-4 N-5)
    Given a 100-node cluster where each node refreshes its inventory every 60 s
    And `KISEKI_CATALOG_BOOT_TIMEOUT_MS` = 90 000 (the rev-4 default)
    And `KISEKI_CATALOG_QUIESCENCE_MS` = 30 000 (the rev-4 default)
    When a fresh node N101 boots and waits for catalog readiness
    Then quiescence is measured against `policy_change_ms` (NOT `inventory_change_ms`)
    And the inventory upserts from the 100 peers (every ~600 ms cluster-wide) do NOT reset the quiescence clock
    And `await_catalog_ready` returns within 30 s + apply-replication-latency
    And `policy_change_ms` is older than 30 s at the moment the resolver runs
