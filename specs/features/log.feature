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

  @integration
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

  @integration
  Scenario: Delta with inline data below threshold (I-L9, ADR-030)
    Given the shard inline threshold is 4096 bytes (per-shard, dynamic — ADR-030)
    When the Composition context appends a delta with:
      | field             | value                        |
      | operation_type    | create                       |
      | encrypted_payload | <1024 bytes, includes inline data> |
    Then the delta is committed with inline data in the payload
    And the payload is offloaded to small/objects.redb on apply (I-SF5)
    And no separate chunk write is required

  @integration
  Scenario: Deltas maintain total order within shard
    Given shard "shard-alpha" has committed deltas with sequence_numbers [1000, 1001, 1002]
    When two deltas are appended concurrently
    Then they are assigned sequence_numbers 1003 and 1004
    And the total order is [1000, 1001, 1002, 1003, 1004]
    And no gaps exist in the sequence

  # --- Failure: Raft leader loss ---

  @integration
  Scenario: Raft leader loss triggers election
    Given node 1 is the Raft leader for "shard-alpha"
    When node 1 becomes unreachable
    Then a new leader is elected from nodes 2 and 3
    And writes resume after election completes
    And in-flight uncommitted deltas are retried by the Composition context
    And no committed deltas are lost

  @integration
  Scenario: Write during leader election is rejected with retriable error
    Given a leader election is in progress for "shard-alpha"
    When the Composition context appends a delta
    Then the append is rejected with a retriable "leader unavailable" error
    And the Composition context retries after backoff

  # --- Failure: Raft quorum loss ---

  @integration
  Scenario: Quorum loss makes shard unavailable for writes
    Given nodes 2 and 3 become unreachable for "shard-alpha"
    And only node 1 (leader) remains
    Then shard "shard-alpha" cannot form a Raft majority
    And all write commands are rejected with "quorum unavailable" error
    And read commands from existing replicas may continue if stale reads are permitted by the view descriptor

  @integration
  Scenario: Quorum recovery resumes normal operation
    Given shard "shard-alpha" lost quorum with only node 1 available
    When node 2 comes back online
    Then quorum is restored (2 of 3)
    And a leader is elected (or confirmed)
    And writes resume
    And the recovered node catches up by replaying missed deltas

  # --- Shard split ---

  @integration
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

  @integration
  Scenario: Split fully wires the new shard end-to-end (I-L6, I-L12, I-L15 — ADR-033)
    Given "shard-alpha" exceeds its hard ceiling
    When the auto-split trigger fires
    Then a new Raft group is formed for "shard-alpha-2" with full RF=3 voter set on three distinct surviving nodes
    And "shard-alpha-2"'s leader is placed per the best-effort round-robin policy (I-L12)
    And the namespace shard map for the affected namespace is atomically updated through the control plane Raft group to record the new range partition (I-L15)
    And the gateway routing cache is invalidated so subsequent writes resolve to the correct shard
    And a write whose hashed_key falls in the new range is committed on "shard-alpha-2" (not on "shard-alpha")
    And no write returns KeyOutOfRange after the split completes

  @integration
  Scenario: Shard split does not block writes
    Given a SplitShard operation is in progress for "shard-alpha"
    When the Composition context appends a delta to "shard-alpha"
    Then the delta is accepted and committed
    And the split operation continues in the background

  # --- Shard merge (I-L13, I-L14 — ADR-034, spec-only) ---

  @integration
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

  @integration
  Scenario: Merge does not block writes (consistent with A-O1, I-O1)
    Given a MergeShard operation is in progress for "shard-c1" and "shard-c2"
    When the Composition context appends a delta whose hashed_key falls in either input range
    Then the delta is accepted and committed
    And the merge operation continues in the background
    And after merge completes, the delta is readable from the merged shard "shard-c12"

  @integration
  Scenario: Concurrent merge and split on the same range is rejected
    Given a MergeShard for "shard-c1" + "shard-c2" has started but not completed
    When a SplitShard is triggered for "shard-c1"
    Then the split is rejected with "shard busy: merge in progress"
    And the merge proceeds to completion
    And the split may be re-evaluated against "shard-c12" after merge completes

  @integration
  Scenario: Merge aborted when tail-chase does not converge (ADV-034-2)
    Given a MergeShard is in progress for "shard-e1" and "shard-e2"
    And both input shards are receiving sustained high write traffic
    When the tail-chase exceeds the convergence timeout (60 seconds)
    Then the merge is aborted
    And the in-progress merged shard is torn down
    And input shards "shard-e1" and "shard-e2" return to state Healthy
    And a MergeAborted event is emitted with reason "convergence_timeout"
    And no writes were lost

  @integration
  Scenario: Merge cutover aborted when tail exceeds budget (ADV-034-2)
    Given a MergeShard has entered cutover (input shards set to read-only)
    And the remaining tail has more than 200 deltas
    When the cutover budget (50ms) would be exceeded
    Then the cutover is aborted
    And input shards are restored to read-write
    And the merged shard is torn down
    And a MergeAborted event is emitted with reason "cutover_budget_exceeded"

  # --- Edge cases ---

  @integration
  Scenario: Delta append to a shard that is splitting
    Given "shard-alpha" is mid-split, creating "shard-alpha-2"
    And the split boundary is at hashed_key 0x8000
    When a delta with hashed_key 0x9000 is appended
    Then the delta is buffered until "shard-alpha-2" is accepting writes
    And a brief write latency bump occurs
    And the delta is committed to "shard-alpha-2" once ready
    And no delta is lost, duplicated, or misplaced

  @integration
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
  #
  # Advisory @unit scenarios (compaction pacing, caller-scoped telemetry,
  # advisory-disabled) moved to crate-level unit tests in:
  #   - crates/kiseki-log/src/store.rs (compaction_works_without_advisory_phase_markers,
  #     shard_health_reports_independent_metrics, advisory_disabled_log_operates_normally)
  # Advisory-specific sub-assertions (MAY defer compaction) are documented
  # as doc comments — the behavior is MAY not MUST.
