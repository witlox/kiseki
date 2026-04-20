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
