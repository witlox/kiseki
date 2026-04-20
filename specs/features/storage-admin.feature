Feature: Storage administration API (ADR-025)

  Cluster admin and storage admin manage pools, tune performance,
  and observe cluster health via the StorageAdminService gRPC API.
  All admin operations are API-first.

  Background:
    Given a Kiseki cluster with pools "fast-nvme" and "bulk-hdd"
    And a cluster admin authenticated with admin mTLS certificate

  # === Pool management ===

  Scenario: Create a new pool
    When the admin creates pool "warm-ssd" with device class "SsdSata" and EC 4+2
    Then the pool appears in ListPools response
    And the pool has zero capacity (no devices assigned yet)

  Scenario: Add devices to pool
    Given pool "warm-ssd" exists with no devices
    When the admin adds devices ["/dev/sda", "/dev/sdb", "/dev/sdc"]
    Then the pool capacity equals the sum of device sizes
    And the pool health is "Healthy"

  Scenario: Change pool durability — rejects if data exists
    Given pool "fast-nvme" has stored chunks
    When the admin attempts to change durability from EC 4+2 to EC 8+3
    Then the operation is rejected with "pool has existing data"
    And a note suggests creating a new pool and migrating

  Scenario: Set pool thresholds
    When the admin sets pool "fast-nvme" warning threshold to 70%
    Then subsequent writes trigger Warning at 70% instead of default 75%

  # === Performance tuning ===

  Scenario: Set cluster-wide compaction rate
    When the admin sets compaction_rate_mb_s to 200
    Then background compaction runs at up to 200 MB/s

  Scenario: Guard rail — compaction rate cannot be zero
    When the admin attempts to set compaction_rate_mb_s to 0
    Then the operation is rejected with "compaction rate must be >= 10"

  Scenario: Set per-pool rebalance target
    When the admin sets pool "fast-nvme" target_fill_pct to 65
    Then the rebalance engine targets 65% fill on each device

  Scenario: Change inline threshold
    When the admin sets inline_threshold_bytes to 8192
    Then new writes under 8KB are inlined in delta payloads
    And existing deltas are unaffected (threshold is prospective)

  Scenario: Tuning parameters inherit — pool overrides cluster
    Given cluster-wide gc_interval_s is 300
    When the admin sets pool "fast-nvme" gc_interval_s to 120
    Then "fast-nvme" runs GC every 120s
    And "bulk-hdd" still runs GC every 300s (cluster default)

  # === Observability ===

  Scenario: Cluster status shows aggregate health
    When the admin requests ClusterStatus
    Then the response includes:
      | field              | type    |
      | node_count         | integer |
      | healthy_nodes      | integer |
      | total_capacity     | bytes   |
      | used_bytes         | bytes   |
      | active_repairs     | integer |
      | evacuating_devices | integer |

  Scenario: Pool status shows performance metrics
    When the admin requests PoolStatus for "fast-nvme"
    Then the response includes read_iops, write_iops, avg_read_latency_ms
    And the metrics reflect the last 60-second window

  Scenario: Device health streaming
    When the admin subscribes to DeviceHealth events
    And a device transitions from Healthy to Degraded
    Then the admin receives a DeviceHealthEvent with old_state and new_state

  Scenario: IO stats streaming
    When the admin subscribes to IOStats for pool "fast-nvme"
    Then the admin receives periodic IOStatsEvent messages
    And each event contains read/write IOPS and throughput

  # === Shard management ===

  Scenario: List all shards
    When the admin requests ListShards
    Then the response includes shard IDs, tenant IDs, and tip sequence numbers

  Scenario: Split shard when approaching ceiling
    Given shard "s1" has 900,000 deltas (ceiling is 1,000,000)
    When the admin triggers SplitShard for "s1"
    Then the shard is split at the key-range midpoint
    And two new shards exist with approximately equal delta counts
    And client writes continue with brief latency bump

  Scenario: Trigger integrity scrub
    When the admin triggers a scrub on pool "fast-nvme"
    Then each chunk's EC integrity is verified
    And corrupted fragments are repaired from parity
    And the scrub result is returned with repair count

  # === Authorization boundary ===

  Scenario: Tenant admin cannot access StorageAdminService
    Given a tenant admin authenticated with tenant certificate
    When they attempt to call ListPools
    Then the request is rejected with PERMISSION_DENIED
    And no pool information is returned

  Scenario: Cluster admin cannot modify tenant quota via StorageAdminService
    Given a cluster admin
    When they attempt to change tenant quota via StorageAdminService
    Then the operation is rejected (tenant quota is via ControlService only)

  Scenario: Admin tuning changes are audited
    When the admin changes compaction_rate_mb_s from 100 to 200
    Then the audit log records:
      | field      | value               |
      | action     | SetTuningParams     |
      | param      | compaction_rate_mb_s |
      | old_value  | 100                 |
      | new_value  | 200                 |
      | admin_id   | cluster-admin-1     |

  # === Operational safety ===

  Scenario: Rebalance is cancellable
    Given a rebalance is in progress on pool "fast-nvme"
    When the admin cancels the rebalance
    Then the rebalance stops gracefully
    And partially moved chunks remain consistent
    And the pool is left in a valid state

  Scenario: Per-tenant resource usage for chargeback
    When the admin requests per-tenant usage summary
    Then the response shows capacity used per tenant
    And IOPS attribution per tenant (last 24h)
    And no tenant can see other tenants' usage

  # === ADR025 adversarial findings — additional scenarios ===

  # C1: Per-tenant usage (via ControlService, not StorageAdmin)
  Scenario: Tenant admin can view their own resource usage
    Given a tenant admin authenticated for "org-pharma"
    When they request GetTenantUsage
    Then the response includes capacity_used_bytes and iops_last_24h
    And only "org-pharma" data is shown

  Scenario: Cluster admin cannot see per-tenant usage via StorageAdminService
    Given a cluster admin
    When they request PoolStatus for "fast-nvme"
    Then the response includes aggregate metrics only
    And no per-tenant breakdown is included

  # C2: Per-device I/O stats
  Scenario: Device I/O stats streaming
    When the admin subscribes to DeviceIOStats for device "dev-1"
    Then the stream includes read_iops, write_iops, read_latency_p50_ms, p99_ms
    And events arrive at least every 5 seconds

  Scenario: Per-device stats reveal load skew
    Given device "dev-1" serves 50k read IOPS and device "dev-2" serves 5k
    When the admin views DeviceIOStats for both
    Then the 10x skew is visible in the metrics

  # C3: Shard health observability
  Scenario: Shard health shows replication status
    When the admin requests GetShardHealth for shard "s1"
    Then the response includes leader_node_id, replica_count, reachable_count
    And commit_lag_entries is reported

  Scenario: Shard health detects degraded replication
    Given shard "s1" has 3 replicas but only 2 are reachable
    When the admin requests GetShardHealth for "s1"
    Then reachable_count is 2 (less than replica_count 3)
    And the admin is alerted to investigate

  # C4: EC parameter immutability
  Scenario: EC parameters cannot be changed on pool with data
    Given pool "fast-nvme" has existing chunks with EC 4+2
    When the admin attempts SetPoolDurability to EC 8+3
    Then the operation applies to new chunks only
    And existing chunks retain EC 4+2

  Scenario: ReencodePool explicitly migrates EC parameters
    Given pool "fast-nvme" has chunks with EC 4+2
    When the admin triggers ReencodePool to EC 8+3
    Then a long-running operation begins
    And progress is reported (chunks re-encoded / total)
    And the operation is cancellable

  # C5: Compaction rate guard rails
  Scenario: Compaction rate cannot be set below minimum
    When the admin attempts to set compaction_rate_mb_s to 5
    Then the operation is rejected with "minimum is 10 MB/s"

  Scenario: Compaction rate change is audited
    When the admin sets compaction_rate_mb_s from 100 to 200
    Then the cluster audit shard contains a TuningParameterChanged event
    And the event includes old_value=100, new_value=200, admin_id

  # C6: Inline threshold is prospective
  Scenario: Inline threshold change does not affect existing deltas
    Given deltas were written with inline_threshold=4096
    When the admin changes inline_threshold to 65536
    Then existing deltas still have 4KB inline payloads
    And new deltas can inline up to 64KB

  # C7: RemoveDevice requires evacuation
  Scenario: RemoveDevice blocked if device has data
    Given device "dev-1" has chunks stored
    When the admin calls RemoveDevice for "dev-1"
    Then the operation fails with DEVICE_NOT_EVACUATED

  Scenario: RemoveDevice succeeds after evacuation
    Given device "dev-1" was evacuated (state = Removed)
    When the admin calls RemoveDevice for "dev-1"
    Then the device is removed from the pool

  # C8: Pool modifications audited to affected tenants
  Scenario: Pool durability change audited to tenant shard
    Given pool "fast-nvme" contains data for tenant "org-pharma"
    When the cluster admin changes pool durability
    Then "org-pharma" tenant audit shard contains a PoolModified event
    And the event includes pool_id, change_type, admin_id

  # C9: Tuning changes audited
  Scenario: All tuning parameter changes are audited
    When the admin changes gc_interval_s from 300 to 120
    Then the cluster audit shard contains:
      | param         | old | new | admin           |
      | gc_interval_s | 300 | 120 | cluster-admin-1 |

  # H1: Streaming buffer semantics
  Scenario: Streaming events have bounded buffer
    When the admin subscribes to DeviceHealth events
    And 20,000 events are generated before the client reads
    Then the oldest events are dropped (buffer capped at 10,000)
    And a StreamOverflowWarning is sent to the client

  # H2: Rebalance cancellation
  Scenario: Rebalance can be cancelled mid-operation
    Given a rebalance is in progress on pool "fast-nvme" at 40%
    When the admin calls CancelRebalance
    Then the rebalance stops
    And already-moved chunks remain in their new locations
    And the pool is in a valid, consistent state

  Scenario: Rebalance progress is observable
    Given a rebalance is in progress
    When the admin calls GetRebalanceProgress
    Then the response includes progress_percent, chunks_moved, estimated_time

  # H3: SplitShard safety
  Scenario: SplitShard rejected if split already in progress
    Given shard "s1" is currently splitting
    When the admin calls SplitShard for "s1"
    Then the operation fails with SPLIT_IN_PROGRESS

  # H5: SRE read-only access
  Scenario: SRE on-call can view cluster status
    Given an SRE authenticated with sre-on-call certificate
    When they request ClusterStatus
    Then the response is returned successfully

  Scenario: SRE on-call cannot modify pool settings
    Given an SRE authenticated with sre-on-call certificate
    When they attempt SetPoolThresholds
    Then the request is rejected with PERMISSION_DENIED

  Scenario: SRE incident-response can trigger scrub
    Given an SRE authenticated with sre-incident-response certificate
    When they call TriggerScrub on pool "fast-nvme"
    Then the scrub begins successfully

  # H6/H7/H8: Multi-tenancy stats leakage documented
  Scenario: Pool stats are aggregate — no tenant breakdown visible
    Given pool "fast-nvme" serves tenants A and B
    When the cluster admin views PoolStatus
    Then read_iops is a combined aggregate
    And there is no way to attribute IOPS to tenant A vs B

  # M4: DrainNode
  Scenario: Drain all devices on a node
    Given node "node-3" has 4 devices in pool "fast-nvme"
    When the admin calls DrainNode for "node-3"
    Then all 4 devices are evacuated in parallel
    And progress is reported per device
    And when complete, all devices are in state "Removed"

  # M5: Rebalance respects capacity thresholds
  Scenario: Rebalance does not push destination pool to ReadOnly
    Given pool "fast-nvme-b" is at 90% (Warning)
    When rebalance tries to move chunks from "fast-nvme-a" to "fast-nvme-b"
    Then rebalance backs off before "fast-nvme-b" reaches Critical
    And the rebalance pauses with a capacity warning
