Feature: Dynamic small-file placement and metadata capacity (ADR-030)

  System disk auto-detection, per-shard inline thresholds, metadata
  capacity management, workload-driven shard placement, and SSD
  read accelerators. The inline threshold determines whether a file's
  encrypted content is stored in small/objects.redb (metadata tier) or
  as a chunk extent on a raw block device (data tier).

  Background:
    Given a Kiseki cluster with 3 nodes:
      | node   | root_disk   | root_size | media_type | data_devices    |
      | node-1 | /dev/nvme0  | 256 GB    | Nvme       | 4x 20TB HDD    |
      | node-2 | /dev/nvme0  | 256 GB    | Nvme       | 4x 20TB HDD    |
      | node-3 | /dev/sda    | 256 GB    | Ssd        | 8x 15TB SSD    |
    And the default metadata limits are:
      | parameter              | value |
      | soft_limit_pct         | 50    |
      | hard_limit_pct         | 75    |
      | inline_floor_bytes     | 128   |
      | inline_ceiling_bytes   | 65536 |
      | raft_inline_mbps       | 10    |

  # === Two-tier redb layout ===

  # === Small-file data path (I-SF5) ===

  @integration
  Scenario: File below threshold stored inline via Raft
    Given shard "shard-1" has inline threshold = 4096 bytes
    When a client writes a 512-byte file
    Then the gateway encrypts the file with envelope encryption
    And the encrypted payload is included in the Raft log entry as delta payload
    And the log entry is replicated to all voters
    And on apply the state machine offloads the payload to small/objects.redb
    And the in-memory state machine retains only the delta header

  @integration
  Scenario: File above threshold stored as chunk extent
    Given shard "shard-1" has inline threshold = 4096 bytes
    When a client writes a 100 KB file
    Then the gateway encrypts the file
    And a chunk extent is allocated on a raw block device
    And the encrypted data is written via O_DIRECT
    And the delta contains only the chunk_ref (no payload)
    And the Raft log entry carries metadata only

  @integration
  Scenario: Read path is transparent — checks redb first, then block
    Given shard "shard-1" has both inline and chunked files
    When a client reads an inline file (chunk_id = "abc123")
    Then ChunkOps::get() finds it in small/objects.redb
    And returns the encrypted content
    When a client reads a chunked file (chunk_id = "def456")
    Then ChunkOps::get() misses in small/objects.redb
    And reads from the block device extent
    And returns the encrypted content

  @integration
  Scenario: Snapshot includes inline content (I-SF5)
    Given shard "shard-1" has 1000 inline files in small/objects.redb
    When the state machine builds a snapshot
    Then the snapshot includes all 1000 inline file contents read from redb
    When a new learner receives this snapshot
    And installs it via install_snapshot
    Then the learner's small/objects.redb contains all 1000 files
    And reads for those files succeed on the learner

  # === Metadata capacity management (I-SF2) ===

  @integration
  Scenario: Emergency signal uses gRPC, not Raft
    Given node-2's disk is at 76% (above hard limit)
    And node-2 cannot write new Raft log entries (disk full for journal)
    When node-2 sends capacity report via data-path gRPC channel
    Then the shard leader receives the report
    And commits threshold reduction using votes from node-1 and node-3
    And node-2 receives the committed change via Raft replication

  # === GC for small/objects.redb (I-SF6) ===

  @integration
  Scenario: Inline file deletion cleans small/objects.redb
    Given an inline file with chunk_id "abc123" exists in small/objects.redb
    When the file is deleted (tombstone delta committed via Raft)
    And all consumer watermarks advance past the tombstone
    And truncate_log or compact_shard runs
    Then the entry for "abc123" is removed from small/objects.redb
    And no orphan entry remains

  @integration
  Scenario: Orphan detection in small/objects.redb
    Given small/objects.redb has 10,000 entries
    And the delta log references only 9,990 of them
    When a scrub or consistency check runs
    Then 10 orphan entries are detected
    And an alert is emitted for investigation

  # === Workload-driven shard placement ===

  @integration
  Scenario: Control plane migrates shard to SSD node (decision tree)
    Given shard "shard-hot" is on node-1 and node-2 (HDD data devices)
    And shard "shard-hot" has small_file_ratio = 0.85
    And shard "shard-hot" p99 read latency = 8ms
    And node-3 has SSD data devices and available capacity
    When the control plane evaluates placement
    Then it determines threshold cannot be lowered further (at floor)
    And shard does not exceed split ceiling
    And node-3 is a better fit (SSD, lower read latency)
    And a migration is initiated:
      | step | action                              |
      | 1    | raft.add_learner(node-3)            |
      | 2    | wait for learner catch-up           |
      | 3    | raft.change_membership (add node-3) |
      | 4    | raft.change_membership (remove node-1 or node-2) |

  @integration
  Scenario: Homogeneous cluster — only threshold and split available
    Given all 3 nodes have identical hardware (NVMe root + HDD data)
    When shard "shard-1" metadata pressure exceeds soft limit
    Then the control plane lowers the inline threshold
    When the threshold is already at floor
    And shard exceeds the I-L6 split ceiling
    Then the shard is split
    When the shard does not exceed split ceiling
    Then an alert is emitted: "metadata tier at capacity, no placement options"

  @integration
  Scenario: Migration has zero downtime
    Given shard "shard-1" is receiving writes at 1000 ops/sec
    When a migration from node-1 to node-3 is in progress
    Then writes continue on the current leader without interruption
    And reads continue from existing voters
    And the new voter (node-3) becomes available after catch-up

  @integration
  Scenario: Failed migration is rolled back
    Given a migration of shard "shard-1" to node-3 is initiated
    And node-3 crashes during learner catch-up
    Then the learner is removed from the Raft group
    And no membership change occurs
    And shard "shard-1" continues operating on original voters

  # === SSD learners as read accelerators ===

  @integration
  Scenario: Add SSD learner for read-heavy shard
    Given shard "shard-1" has RF=3 voters on HDD nodes
    And shard "shard-1" has high read IOPS for small files
    When an SSD learner is added to shard "shard-1"
    Then the learner receives the full Raft log
    And its small/objects.redb is populated via log replay
    And read requests can be served from the SSD learner
    And the learner does NOT participate in elections
    And the learner does NOT count toward commit quorum

  @integration
  Scenario: Learner promoted to voter when workload persists
    Given shard "shard-1" has an SSD learner serving reads for 24 hours
    And the small-file workload persists
    When the control plane promotes the SSD learner to voter
    And demotes an HDD voter
    Then the shard has an SSD voter and improved write latency
    And the old HDD voter's data is eventually GC'd

