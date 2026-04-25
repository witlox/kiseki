Feature: View Materialization — Stream processors maintaining protocol-shaped views
  The View Materialization context consumes deltas from shards and
  maintains materialized views per view descriptor. Stream processors
  are per-tenant (in the tenant trust domain, cache tenant key material).
  Views are rebuildable from the log.

  Background:
    Given a Kiseki cluster with tenant "org-pharma"
    And shard "shard-trials-1" with committed deltas up to sequence 5000
    And view descriptor "nfs-trials":
      | field             | value              |
      | source_shards     | [shard-trials-1]   |
      | protocol          | POSIX              |
      | consistency       | read-your-writes   |
      | affinity_pool     | fast-nvme          |
      | discardable       | true               |
    And view descriptor "s3-trials":
      | field             | value              |
      | source_shards     | [shard-trials-1]   |
      | protocol          | S3                 |
      | consistency       | bounded-staleness  |
      | staleness_bound   | 5s                 |
      | affinity_pool     | bulk-nvme          |
      | discardable       | true               |
    And a HIPAA compliance floor of 2s staleness

  # --- Happy path: incremental materialization ---

  @integration
  Scenario: Stream processor consumes deltas and updates NFS view
    Given stream processor "sp-nfs-trials" is at watermark 4990
    When new deltas [4991..5000] are available in "shard-trials-1"
    Then "sp-nfs-trials" reads deltas 4991 to 5000
    And decrypts each delta payload using cached tenant KEK
    And applies the mutations to the materialized POSIX directory tree
    And advances its watermark to 5000
    And the NFS view reflects state as of sequence 5000

  @integration
  Scenario: POSIX view provides read-your-writes
    Given the NFS view is at watermark 5000
    And a new delta (sequence 5001) is committed by a write through NFS
    When a read arrives through the NFS protocol gateway
    Then the stream processor applies delta 5001 before serving the read
    And the reader sees the write that was just committed
    And this guarantee holds for reads through the same protocol

  # --- View lifecycle ---

  @integration
  Scenario: Create a new view
    Given tenant admin creates view descriptor "analytics-trials":
      | field             | value              |
      | source_shards     | [shard-trials-1]   |
      | protocol          | S3                 |
      | consistency       | bounded-staleness  |
      | staleness_bound   | 60s                |
      | affinity_pool     | bulk-nvme          |
      | discardable       | true               |
    When the Control Plane registers the descriptor
    Then a new stream processor "sp-analytics-trials" is spawned
    And it begins consuming from shard-trials-1 at position 0
    And it materializes the view from the beginning of the log
    And it catches up to the current log tip over time

  @integration
  Scenario: Discard and rebuild a view
    Given view "s3-trials" is discardable and occupies 500GB on bulk-nvme
    When the cluster admin (with tenant admin approval) discards the view
    Then the materialized state is deleted from bulk-nvme
    And the stream processor is stopped
    And the view descriptor is retained
    And later, the view can be rebuilt by restarting the stream processor
    And it re-materializes from the log (position 0)

  @integration
  Scenario: View descriptor version change — pull-based propagation
    Given stream processor "sp-nfs-trials" is running
    When the tenant admin updates descriptor "nfs-trials" to change affinity_pool to "bulk-nvme"
    Then a new descriptor version is stored in the Control Plane
    And on the next materialization cycle, "sp-nfs-trials" detects the new version
    And it begins materializing new state in "bulk-nvme"
    And it migrates existing materialized data in background
    And reads continue from old materialization until migration completes

  # --- Failure paths ---

  @integration
  Scenario: Stream processor crashes — recovery from last watermark
    Given stream processor "sp-nfs-trials" crashes at watermark 4500
    When it restarts
    Then it reads its last persisted watermark (4500) from durable storage
    And resumes consuming from position 4501
    And re-materializes deltas [4501..current] into the view
    And no data is lost or duplicated (idempotent application)

  @integration
  Scenario: Stream processor cannot decrypt — tenant key unavailable
    Given "sp-nfs-trials" cached tenant KEK expires
    And tenant KMS is unreachable
    When new deltas arrive
    Then the stream processor stalls at its current watermark
    And the view becomes stale (falls behind the staleness bound)
    And alerts are raised to cluster admin (view stalled) and tenant admin (KMS issue)
    And when KMS becomes reachable, the processor resumes and catches up

  @integration
  Scenario: Source shard unavailable — view serves last known state
    Given shard "shard-trials-1" loses Raft quorum
    When the stream processor cannot read new deltas
    Then the view continues serving reads from its last materialized state
    And reads are marked as potentially stale
    And no new writes can be reflected until the shard recovers

  # --- Workflow Advisory integration (ADR-020) ---
  # View Materialization acts on prefetch-range, access-pattern, and phase
  # hints from callers and emits materialization-lag and pin-headroom
  # telemetry scoped strictly to the caller's own views (I-WA5, I-WA6).
  # Hints never change consistency-model or compliance-floor enforcement
  # (I-WA14, I-K9).

  @unit
  Scenario: Prefetch-range hint warms caller's view opportunistically
    Given workload "training-run-42" has an active workflow in phase "epoch-0"
    And the workflow has submitted a PrefetchHint of 4096 (composition_id, offset, length) tuples into view "nfs-trials"
    When the stream processor has idle materialization capacity
    Then it MAY decrypt + cache chunk data for the declared ranges in advance of read requests
    And MUST NOT advance its public watermark past its normal rules (I-V2)
    And MUST NOT decrypt payloads outside the caller's tenant scope (I-T1)
    And prefetch work is preempted by genuine read requests or compaction pressure

  @unit
  Scenario: Access-pattern hint { random } suppresses readahead
    Given the stream processor normally performs sequential readahead for POSIX views
    And the caller submits hint { access_pattern: random } for view "nfs-trials"
    When subsequent reads arrive
    Then the readahead heuristic is disabled for this caller's reads
    And cache residency policy shifts toward per-chunk LRU rather than sequential warm-forward
    And other callers' reads on the same view are unaffected (steering is caller-scoped)

  @unit
  Scenario: Phase marker { checkpoint } biases cache retention
    Given the workflow advances to phase "checkpoint" with profile hpc-checkpoint
    When the stream processor observes the phase marker on subsequent reads/writes
    Then cache retention for checkpoint-target compositions is extended within policy bounds
    And cache eviction preferentially targets non-checkpoint compositions of the same caller
    And cross-tenant cache state is not affected (I-T1)

  @unit
  Scenario: Materialization-lag telemetry scoped to caller's views
    Given workload "training-run-42" owns views "nfs-trials" and "s3-trials"
    And a neighbour workload owns view "nfs-other"
    When the caller subscribes to materialization-lag telemetry
    Then the stream returns lag values for "nfs-trials" and "s3-trials" only
    And attempts to subscribe to "nfs-other" return not_found with shape identical to absent views (I-WA6)
    And the numeric lag values are reported in bucketed milliseconds (no fine-grained timing leak)

  @unit
  Scenario: Pin-headroom telemetry
    Given workload "training-run-42" holds 80% of its allowed MVCC pins (I-V4)
    When the caller subscribes to pin-headroom telemetry
    Then a bucketed value ("ample" | "approaching-limit" | "near-exhaustion") is returned
    And no absolute pin counts or neighbour-workload pin state is exposed (I-WA5)

  @unit
  Scenario: Advisory opt-out on workload — view stops accepting hints, continues serving reads
    Given tenant admin transitions "training-run-42" advisory to disabled
    When the stream processor receives no new hints for this workload
    Then existing materialization and read paths continue unchanged (I-WA2)
    And any pre-declared prefetch ranges for this workload are abandoned (not retained across disable)
    And correctness of views served to the workload is unaffected
