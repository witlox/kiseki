Feature: Log — Delta ordering, replication, and shard lifecycle
  The Log context accepts deltas from the Composition context, assigns
  total ordering within a shard via Raft, replicates for durability,
  and manages shard lifecycle (split, compaction, truncation).

  Background:
    Given a Kiseki cluster with 5 storage nodes
    And a shard "shard-alpha" with a 3-member Raft group on nodes 1, 2, 3
    And node 1 is the Raft leader for "shard-alpha"
    And tenant "org-pharma" exists with an active tenant KMS

  # --- Happy path: delta append ---

  Scenario: Successful delta append
    Given shard "shard-alpha" is healthy with all 3 replicas online
    When the Composition context appends a delta with:
      | field           | value                          |
      | tenant_id       | org-pharma                     |
      | operation_type  | create                         |
      | hashed_key      | sha256(parent_dir_id + name)   |
      | encrypted_payload | <tenant-encrypted blob>      |
    Then the delta is assigned sequence_number 1001
    And the delta is replicated to at least 2 of 3 Raft members
    And a DeltaCommitted event is emitted with sequence_number 1001
    And the commit_ack is returned to the Composition context

  Scenario: Delta with inline data below threshold (I-L9, ADR-030)
    Given the shard inline threshold is 4096 bytes (per-shard, dynamic — ADR-030)
    When the Composition context appends a delta with:
      | field             | value                        |
      | operation_type    | create                       |
      | encrypted_payload | <1024 bytes, includes inline data> |
    Then the delta is committed with inline data in the payload
    And the payload is offloaded to small/objects.redb on apply (I-SF5)
    And no separate chunk write is required

  Scenario: Deltas maintain total order within shard
    Given shard "shard-alpha" has committed deltas with sequence_numbers [1000, 1001, 1002]
    When two deltas are appended concurrently
    Then they are assigned sequence_numbers 1003 and 1004
    And the total order is [1000, 1001, 1002, 1003, 1004]
    And no gaps exist in the sequence

  # --- Failure: Raft leader loss ---

  Scenario: Raft leader loss triggers election
    Given node 1 is the Raft leader for "shard-alpha"
    When node 1 becomes unreachable
    Then a new leader is elected from nodes 2 and 3
    And writes resume after election completes
    And in-flight uncommitted deltas are retried by the Composition context
    And no committed deltas are lost

  Scenario: Write during leader election is rejected with retriable error
    Given a leader election is in progress for "shard-alpha"
    When the Composition context appends a delta
    Then the append is rejected with a retriable "leader unavailable" error
    And the Composition context retries after backoff

  # --- Failure: Raft quorum loss ---

  Scenario: Quorum loss makes shard unavailable for writes
    Given nodes 2 and 3 become unreachable for "shard-alpha"
    And only node 1 (leader) remains
    Then shard "shard-alpha" cannot form a Raft majority
    And all write commands are rejected with "quorum unavailable" error
    And read commands from existing replicas may continue if stale reads are permitted by the view descriptor

  Scenario: Quorum recovery resumes normal operation
    Given shard "shard-alpha" lost quorum with only node 1 available
    When node 2 comes back online
    Then quorum is restored (2 of 3)
    And a leader is elected (or confirmed)
    And writes resume
    And the recovered node catches up by replaying missed deltas

  # --- Shard split ---

  Scenario: Shard split triggered by hard ceiling (I-L6)
    Given the hard ceiling for "shard-alpha" is:
      | dimension    | threshold  |
      | delta_count  | 10000000   |
      | byte_size    | 10GB       |
    And "shard-alpha" has 10000001 deltas
    Then a SplitShard operation is triggered automatically
    And a new shard "shard-alpha-2" is created
    And new deltas are routed to the appropriate shard by hashed_key range
    And "shard-alpha" continues serving reads for its existing range
    And a ShardSplit event is emitted

  Scenario: Split fully wires the new shard end-to-end (I-L6, I-L12, I-L15 — ADR-033)
    Given "shard-alpha" exceeds its hard ceiling
    When the auto-split trigger fires
    Then a new Raft group is formed for "shard-alpha-2" with full RF=3 voter set on three distinct surviving nodes
    And "shard-alpha-2"'s leader is placed per the best-effort round-robin policy (I-L12)
    And the namespace shard map for the affected namespace is atomically updated through the control plane Raft group to record the new range partition (I-L15)
    And the gateway routing cache is invalidated so subsequent writes resolve to the correct shard
    And a write whose hashed_key falls in the new range is committed on "shard-alpha-2" (not on "shard-alpha")
    And no write returns KeyOutOfRange after the split completes

  Scenario: Shard split does not block writes
    Given a SplitShard operation is in progress for "shard-alpha"
    When the Composition context appends a delta to "shard-alpha"
    Then the delta is accepted and committed
    And the split operation continues in the background

  # --- Shard merge (I-L13, I-L14 — ADR-034, spec-only) ---

  Scenario: Adjacent shards merge when sustained underutilization is observed
    Given namespace "ns-c" has shards "shard-c1" (range [0x0000, 0x4000)) and "shard-c2" (range [0x4000, 0x8000))
    And both shards have been below 25% of every split-ceiling dimension for the past 24 hours
    And merging them would not violate the ratio floor (I-L11)
    Then a MergeShard operation is triggered automatically
    And a new shard "shard-c12" with range [0x0000, 0x8000) is created
    And total order is preserved across the merged range (I-L14)
    And "shard-c1" and "shard-c2" are retired after the merge HLC timestamp
    And a ShardMerged event is emitted recording the input IDs, output ID, range, and merge HLC
    And the namespace shard map is updated atomically (I-L15)

  Scenario: Merge refused when ratio floor would be violated
    Given namespace "ns-d" has 5 shards on a 3-node cluster (ratio = 1.67)
    And shards "shard-d1" and "shard-d2" are merge-eligible by utilization
    When the merge candidate evaluator runs
    Then the merge is NOT triggered, because merging would yield ratio = 4/3 ≈ 1.33 < 1.5
    And a MergeRefused event is recorded with reason "ratio_floor_would_be_violated"

  Scenario: Merge does not block writes (consistent with A-O1, I-O1)
    Given a MergeShard operation is in progress for "shard-c1" and "shard-c2"
    When the Composition context appends a delta whose hashed_key falls in either input range
    Then the delta is accepted and committed
    And the merge operation continues in the background
    And after merge completes, the delta is readable from the merged shard "shard-c12"

  Scenario: Concurrent merge and split on the same range is rejected
    Given a MergeShard for "shard-c1" + "shard-c2" has started but not completed
    When a SplitShard is triggered for "shard-c1"
    Then the split is rejected with "shard busy: merge in progress"
    And the merge proceeds to completion
    And the split may be re-evaluated against "shard-c12" after merge completes

  # --- Compaction ---

  Scenario: Automatic compaction merges SSTables
    Given shard "shard-alpha" has 20 unmerged SSTables
    And the compaction threshold is 10 SSTables
    When automatic compaction is triggered
    Then SSTables are merged by hashed_key and sequence_number
    And newer deltas (higher sequence_number) supersede older ones
    And tombstoned entries are removed if all consumers have advanced past them
    And tenant-encrypted payloads are carried opaquely — never decrypted
    And the resulting SSTable count is reduced

  Scenario: Admin-triggered compaction
    Given the cluster admin triggers compaction on "shard-alpha"
    Then compaction runs regardless of the automatic threshold
    And the same merge semantics apply
    And the operation is recorded in the audit log

  # --- Truncation / GC ---

  Scenario: Delta GC respects all consumer watermarks
    Given shard "shard-alpha" has deltas from sequence 1 to 10000
    And stream processor "sp-nfs" has consumed up to sequence 9500
    And stream processor "sp-s3" has consumed up to sequence 8000
    And the audit log has consumed up to sequence 9000
    When TruncateLog runs
    Then deltas up to sequence 7999 are eligible for GC
    And deltas from 8000 onward are retained
    And the minimum consumer watermark (8000) determines the GC boundary

  Scenario: Stalled consumer blocks GC
    Given stream processor "sp-analytics" has stalled at sequence 1000
    And all other consumers have advanced past sequence 50000
    When TruncateLog runs
    Then no deltas after sequence 999 are GC'd
    And an alert is raised to the cluster admin (GC blocked)
    And an alert is raised to the tenant admin (view is stale)

  # --- Maintenance mode ---

  Scenario: Maintenance mode rejects writes
    Given the cluster admin sets "shard-alpha" to maintenance mode
    Then a ShardMaintenanceEntered event is emitted
    And all AppendDelta commands are rejected with retriable "read-only" error
    And ReadDeltas queries continue to work
    And ShardHealth queries continue to work

  Scenario: Exiting maintenance mode resumes writes
    Given "shard-alpha" is in maintenance mode
    When the cluster admin clears maintenance mode
    Then AppendDelta commands are accepted again
    And if "shard-alpha" was at the hard ceiling, SplitShard triggers immediately

  # --- Range read for view materialization ---

  Scenario: Stream processor reads delta range
    Given shard "shard-alpha" has deltas from sequence 1 to 5000
    When a stream processor reads deltas from position 4000 to 5000
    Then it receives deltas [4000, 4001, ..., 5000] in order
    And each delta includes the full envelope (header + encrypted payload)
    And the stream processor decrypts payloads using cached tenant key material

  # --- Edge cases ---

  Scenario: Delta append to a shard that is splitting
    Given "shard-alpha" is mid-split, creating "shard-alpha-2"
    And the split boundary is at hashed_key 0x8000
    When a delta with hashed_key 0x9000 is appended
    Then the delta is buffered until "shard-alpha-2" is accepting writes
    And a brief write latency bump occurs
    And the delta is committed to "shard-alpha-2" once ready
    And no delta is lost, duplicated, or misplaced

  Scenario: Concurrent split and compaction
    Given "shard-alpha" is being compacted
    And a SplitShard is triggered during compaction
    Then both operations proceed
    And compaction completes on the pre-split key range
    And the split creates a new shard with its own compaction state

  # --- Workflow Advisory integration (ADR-020) ---
  # The Log is largely passive to the advisory concern — phase markers
  # are advisory hints routed upstream only as heuristics for compaction
  # pacing and shard-side tuning. The Log emits caller-scoped shard
  # saturation telemetry. Hints never change Raft ordering, durability,
  # or compaction correctness (I-L1/L2/L3, I-WA1).

  Scenario: Phase marker { checkpoint } may inform compaction pacing
    Given workload "training-run-42" advances its workflow to phase "checkpoint"
    And compositions on "shard-trials-1" are written heavily during this phase
    When the compaction pacer observes the phase-marker heuristic
    Then it MAY defer aggressive compaction on "shard-trials-1" during the checkpoint burst
    And compaction MUST resume to honour its configured thresholds regardless of hints (I-L6)
    And the hint never affects delta ordering, durability, or GC correctness (I-WA1)

  Scenario: Shard saturation telemetry is caller-scoped
    Given workload "training-run-42" has compositions on "shard-trials-1" (owned) and a neighbour workload has compositions on the same shard
    When the caller subscribes to shard-saturation telemetry for "shard-trials-1"
    Then the returned backpressure signal reflects only the caller's own append rate and commit latency for that shard
    And neighbour workloads' contribution is not inferable (I-WA5)
    And requesting telemetry for a shard with no caller-owned compositions returns the same shape as a nonexistent shard (I-WA6)

  Scenario: Advisory disabled — log serves all tenants normally
    Given advisory is disabled cluster-wide
    When workloads append deltas, trigger shard splits, and run compaction
    Then all Log operations succeed with full correctness and durability (I-WA2)
    And no compaction pacing heuristic uses absent advisory signals (behaves as if no phase markers were present)
