Feature: Storage administration API (ADR-025)

  Cluster admin and storage admin manage pools, tune performance,
  and observe cluster health via the StorageAdminService gRPC API.
  All admin operations are API-first.

  Background:
    Given a Kiseki cluster with pools "fast-nvme" and "bulk-hdd"
    And a cluster admin authenticated with admin mTLS certificate

  # === Pool management ===

  @integration
  Scenario: Add devices to pool
    Given pool "warm-ssd" exists with no devices
    When the admin adds devices ["/dev/sda", "/dev/sdb", "/dev/sdc"]
    Then the pool capacity equals the sum of device sizes
    And the pool health is "Healthy"

  # @unit scenarios moved to crate-level unit tests:
  # "Change pool durability" → kiseki-control/src/storage_admin.rs::change_pool_durability_rejects_with_data
  # "Set pool thresholds" → kiseki-control/src/storage_admin.rs::set_pool_warning_threshold

  # === Performance tuning ===

  # @unit scenarios moved to crate-level unit tests:
  # "Set cluster-wide compaction rate" → kiseki-control/src/storage_admin.rs::set_compaction_rate
  # "Guard rail — compaction rate cannot be zero" → kiseki-control/src/storage_admin.rs::compaction_rate_minimum_guard
  # "Set per-pool rebalance target" → kiseki-control/src/storage_admin.rs::rebalance_target_per_pool
  # "Tuning parameters inherit" → kiseki-control/src/storage_admin.rs::tuning_params_pool_overrides_cluster

  # === Observability ===

  # @unit scenario "Pool status shows performance metrics" → kiseki-control/src/storage_admin.rs::pool_status_includes_metrics

  @integration
  Scenario: Device health streaming
    When the admin subscribes to DeviceHealth events
    And a device transitions from Healthy to Degraded
    Then the admin receives a DeviceHealthEvent with old_state and new_state

  @integration
  Scenario: IO stats streaming
    When the admin subscribes to IOStats for pool "fast-nvme"
    Then the admin receives periodic IOStatsEvent messages
    And each event contains read/write IOPS and throughput

  # === Shard management ===

  @integration
  Scenario: Split shard when approaching ceiling
    Given shard "s1" has 900,000 deltas (ceiling is 1,000,000)
    When the admin triggers SplitShard for "s1"
    Then the shard is split at the key-range midpoint
    And two new shards exist with approximately equal delta counts
    And client writes continue with brief latency bump

  @integration
  Scenario: Trigger integrity scrub
    When the admin triggers a scrub on pool "fast-nvme"
    Then each chunk's EC integrity is verified
    And corrupted fragments are repaired from parity
    And the scrub result is returned with repair count

  # === Authorization boundary ===

  # @unit scenario "Admin tuning changes are audited" → kiseki-control/src/storage_admin.rs::tuning_changes_audited

  # === Operational safety ===

  @integration
  Scenario: Rebalance is cancellable
    Given a rebalance is in progress on pool "fast-nvme"
    When the admin cancels the rebalance
    Then the rebalance stops gracefully
    And partially moved chunks remain consistent
    And the pool is left in a valid state

  # @unit scenario "Per-tenant resource usage" → kiseki-control/src/storage_admin.rs::per_tenant_usage_isolation

  # === ADR025 adversarial findings — additional scenarios ===

  # C1: Per-tenant usage (via ControlService, not StorageAdmin)
  # @unit scenario "Tenant admin views own usage" → kiseki-control/src/storage_admin.rs::tenant_admin_own_usage

  # C2: Per-device I/O stats
  @integration
  Scenario: Device I/O stats streaming
    When the admin subscribes to DeviceIOStats for device "dev-1"
    Then the stream includes read_iops, write_iops, read_latency_p50_ms, p99_ms
    And events arrive at least every 5 seconds

  @integration
  Scenario: Per-device stats reveal load skew
    Given device "dev-1" serves 50k read IOPS and device "dev-2" serves 5k
    When the admin views DeviceIOStats for both
    Then the 10x skew is visible in the metrics

  # C3: Shard health observability
  @integration
  Scenario: Shard health shows replication status
    When the admin requests GetShardHealth for shard "s1"
    Then the response includes leader_node_id, replica_count, reachable_count
    And commit_lag_entries is reported

  @integration
  Scenario: Shard health detects degraded replication
    Given shard "s1" has 3 replicas but only 2 are reachable
    When the admin requests GetShardHealth for "s1"
    Then reachable_count is 2 (less than replica_count 3)
    And the admin is alerted to investigate

  # C4: EC parameter immutability
  # @unit scenario "EC parameters cannot be changed" → kiseki-control/src/storage_admin.rs::ec_immutability_existing_chunks

  @integration
  Scenario: ReencodePool explicitly migrates EC parameters
    Given pool "fast-nvme" has chunks with EC 4+2
    When the admin triggers ReencodePool to EC 8+3
    Then a long-running operation begins
    And progress is reported (chunks re-encoded / total)
    And the operation is cancellable

  # C5: Compaction rate guard rails
  # @unit scenarios moved to crate-level unit tests:
  # "Compaction rate below minimum" → kiseki-control/src/storage_admin.rs::compaction_rate_minimum_guard
  # "Compaction rate change is audited" → kiseki-control/src/storage_admin.rs::compaction_rate_change_audited

  # C6: Inline threshold is prospective
  # @unit scenario → kiseki-control/src/storage_admin.rs::inline_threshold_prospective

  # C7: RemoveDevice requires evacuation
  @integration
  Scenario: RemoveDevice blocked if device has data
    Given device "dev-1" has chunks stored
    When the admin calls RemoveDevice for "dev-1"
    Then the operation fails with DEVICE_NOT_EVACUATED

  @integration
  Scenario: RemoveDevice succeeds after evacuation
    Given device "dev-1" was evacuated (state = Removed)
    When the admin calls RemoveDevice for "dev-1"
    Then the device is removed from the pool

  # C8: Pool modifications audited to affected tenants
  # @unit scenario "Pool durability change audited" → kiseki-control/src/storage_admin.rs::pool_durability_change_audited_to_tenant

  # C9: Tuning changes audited
  # @unit scenario "All tuning parameter changes" → kiseki-control/src/storage_admin.rs::all_tuning_params_audited

  # H1: Streaming buffer semantics
  @integration
  Scenario: Streaming events have bounded buffer
    When the admin subscribes to DeviceHealth events
    And 20,000 events are generated before the client reads
    Then the oldest events are dropped (buffer capped at 10,000)
    And a StreamOverflowWarning is sent to the client

  # H2: Rebalance cancellation
  @integration
  Scenario: Rebalance can be cancelled mid-operation
    Given a rebalance is in progress on pool "fast-nvme" at 40%
    When the admin calls CancelRebalance
    Then the rebalance stops
    And already-moved chunks remain in their new locations
    And the pool is in a valid, consistent state

  @integration
  Scenario: Rebalance progress is observable
    Given a rebalance is in progress
    When the admin calls GetRebalanceProgress
    Then the response includes progress_percent, chunks_moved, estimated_time

  # H3: SplitShard safety
  @integration
  Scenario: SplitShard rejected if split already in progress
    Given shard "s1" is currently splitting
    When the admin calls SplitShard for "s1"
    Then the operation fails with SPLIT_IN_PROGRESS

  # H5: SRE read-only access
  # @unit scenarios moved to crate-level unit tests:
  # "SRE on-call can view cluster status" → kiseki-control/src/storage_admin.rs::sre_can_view_cluster_status
  # "SRE on-call cannot modify pool settings" → kiseki-control/src/storage_admin.rs::sre_cannot_create_pool

  @integration
  Scenario: SRE incident-response can trigger scrub
    Given an SRE authenticated with sre-incident-response certificate
    When they call TriggerScrub on pool "fast-nvme"
    Then the scrub begins successfully

  # H6/H7/H8: Multi-tenancy stats leakage documented
  # @unit scenario "Pool stats are aggregate" → kiseki-control/src/storage_admin.rs::pool_status_has_only_aggregate_fields

  # M4: DrainNode
  @integration
  Scenario: Drain all devices on a node
    Given node "node-3" has 4 devices in pool "fast-nvme"
    When the admin calls DrainNode for "node-3"
    Then all 4 devices are evacuated in parallel
    And progress is reported per device
    And when complete, all devices are in state "Removed"

  # M5: Rebalance respects capacity thresholds
  @integration
  Scenario: Rebalance does not push destination pool to ReadOnly
    Given pool "fast-nvme-b" is at 90% (Warning)
    When rebalance tries to move chunks from "fast-nvme-a" to "fast-nvme-b"
    Then rebalance backs off before "fast-nvme-b" reaches Critical
    And the rebalance pauses with a capacity warning
