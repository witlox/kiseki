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

  Scenario: Stream processor consumes deltas and updates NFS view
    Given stream processor "sp-nfs-trials" is at watermark 4990
    When new deltas [4991..5000] are available in "shard-trials-1"
    Then "sp-nfs-trials" reads deltas 4991 to 5000
    And decrypts each delta payload using cached tenant KEK
    And applies the mutations to the materialized POSIX directory tree
    And advances its watermark to 5000
    And the NFS view reflects state as of sequence 5000

  Scenario: Stream processor respects staleness bound
    Given stream processor "sp-s3-trials" is at watermark 4950
    And the effective staleness bound is 2s (HIPAA floor overrides 5s descriptor)
    When 2 seconds have elapsed since watermark 4950's timestamp
    Then "sp-s3-trials" MUST consume available deltas to stay within bound
    And if deltas are available, it advances to at least the delta within 2s
    And if no deltas exist in that window, the view is current

  Scenario: POSIX view provides read-your-writes
    Given the NFS view is at watermark 5000
    And a new delta (sequence 5001) is committed by a write through NFS
    When a read arrives through the NFS protocol gateway
    Then the stream processor applies delta 5001 before serving the read
    And the reader sees the write that was just committed
    And this guarantee holds for reads through the same protocol

  # --- View lifecycle ---

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

  Scenario: Discard and rebuild a view
    Given view "s3-trials" is discardable and occupies 500GB on bulk-nvme
    When the cluster admin (with tenant admin approval) discards the view
    Then the materialized state is deleted from bulk-nvme
    And the stream processor is stopped
    And the view descriptor is retained
    And later, the view can be rebuilt by restarting the stream processor
    And it re-materializes from the log (position 0)

  Scenario: View descriptor version change — pull-based propagation
    Given stream processor "sp-nfs-trials" is running
    When the tenant admin updates descriptor "nfs-trials" to change affinity_pool to "bulk-nvme"
    Then a new descriptor version is stored in the Control Plane
    And on the next materialization cycle, "sp-nfs-trials" detects the new version
    And it begins materializing new state in "bulk-nvme"
    And it migrates existing materialized data in background
    And reads continue from old materialization until migration completes

  # --- MVCC reads ---

  Scenario: MVCC read pins a log position
    Given the NFS view is at watermark 5000
    When a read operation begins
    Then it pins a snapshot at position 5000
    And concurrent writes (position 5001, 5002) are invisible to this read
    And the read sees a consistent point-in-time snapshot

  Scenario: MVCC pin expires — read must restart or complete
    Given a read pinned at position 3000 has been active for 600 seconds
    And the pin TTL for this view is 300 seconds
    When the pin expires
    Then the snapshot guarantee is revoked
    And the read receives a "snapshot expired" error if still in progress
    And the caller may restart the read from a fresher position
    And compaction can now proceed past position 3000

  # --- Object versioning ---

  Scenario: View exposes object versions
    Given namespace "trials" has versioning enabled
    And composition "results.h5" has been written 3 times (v1, v2, v3)
    When the S3 view lists versions for "results.h5"
    Then it returns [v1, v2, v3] with their respective log positions
    And each version is independently readable
    And the current version is v3

  Scenario: Version read at historical position
    Given "results.h5" v1 was committed at log position 1000
    And v2 at position 2000, v3 at position 3000
    When a read requests version v1 specifically
    Then the view returns the state of "results.h5" at position 1000
    And chunks referenced by v1 are read from Chunk Storage
    And the read does not require replaying the log (view has version index)

  # --- Cross-view consistency ---

  Scenario: Write via NFS, read via S3 — bounded staleness
    Given a write through NFS commits at sequence 5001
    And the NFS view reflects 5001 immediately (read-your-writes)
    And the S3 view is at watermark 4999 (within 2s HIPAA floor)
    When a read arrives through S3 for the same data
    Then the S3 view may NOT reflect 5001 yet (staleness within bound)
    And the reader sees state as of 4999
    And this is compliant because S3 declares bounded-staleness

  Scenario: Write via NFS, read via NFS — read-your-writes
    Given a write through NFS commits at sequence 5001
    When a read arrives through NFS for the same data
    Then the NFS view reflects 5001 (read-your-writes guarantee)
    And the reader sees their own write

  # --- Failure paths ---

  Scenario: Stream processor crashes — recovery from last watermark
    Given stream processor "sp-nfs-trials" crashes at watermark 4500
    When it restarts
    Then it reads its last persisted watermark (4500) from durable storage
    And resumes consuming from position 4501
    And re-materializes deltas [4501..current] into the view
    And no data is lost or duplicated (idempotent application)

  Scenario: Stream processor cannot decrypt — tenant key unavailable
    Given "sp-nfs-trials" cached tenant KEK expires
    And tenant KMS is unreachable
    When new deltas arrive
    Then the stream processor stalls at its current watermark
    And the view becomes stale (falls behind the staleness bound)
    And alerts are raised to cluster admin (view stalled) and tenant admin (KMS issue)
    And when KMS becomes reachable, the processor resumes and catches up

  Scenario: Stream processor falls behind — staleness violation
    Given "sp-s3-trials" is at watermark 4000
    And the effective staleness bound is 2s
    And 10 seconds have elapsed since watermark 4000
    Then the staleness bound is violated
    And alerts are raised to both cluster admin and tenant admin
    And reads from the S3 view may optionally return a "stale data" warning header
    And the stream processor continues catching up as fast as possible

  Scenario: Source shard unavailable — view serves last known state
    Given shard "shard-trials-1" loses Raft quorum
    When the stream processor cannot read new deltas
    Then the view continues serving reads from its last materialized state
    And reads are marked as potentially stale
    And no new writes can be reflected until the shard recovers
