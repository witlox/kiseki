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

  # === System disk auto-detection (I-SF2) ===

  Scenario: NVMe root disk detected — no warning
    Given node-1 boots with KISEKI_DATA_DIR="/data"
    When the server detects /sys/block/nvme0/queue/rotational = 0
    Then the node reports media_type = "Nvme"
    And soft_limit_bytes = 128 GB (50% of 256 GB)
    And hard_limit_bytes = 192 GB (75% of 256 GB)
    And no rotational warning is emitted

  Scenario: HDD root disk detected — warning emitted
    Given a node boots with root disk on /dev/sdb (rotational = 1)
    When the server detects /sys/block/sdb/queue/rotational = 1
    Then the node reports media_type = "Hdd"
    And a persistent warning is emitted:
      """
      WARNING: system disk is rotational (HDD). Raft fsync latency will
      be 5-10ms per commit. Production deployments require NVMe or SSD
      for the metadata partition. See ADR-030.
      """
    And the warning appears in health reports

  Scenario: Custom soft/hard limits via environment
    Given KISEKI_META_SOFT_LIMIT_PCT=60 and KISEKI_META_HARD_LIMIT_PCT=80
    When node-1 boots with a 256 GB root disk
    Then soft_limit_bytes = 153 GB (60% of 256 GB)
    And hard_limit_bytes = 204 GB (80% of 256 GB)

  # === Two-tier redb layout ===

  Scenario: Metadata and small-file stores are separate redb databases
    Given node-1 is running with KISEKI_DATA_DIR="/data"
    Then the following redb files exist:
      | path                     | purpose                       |
      | /data/raft/log.redb      | Raft log entries              |
      | /data/keys/epochs.redb   | Key epoch metadata            |
      | /data/chunks/meta.redb   | Chunk extent index            |
      | /data/small/objects.redb | Small-file encrypted content  |

  # === Per-shard dynamic inline threshold (I-SF1) ===

  Scenario: Threshold computed from minimum voter budget
    Given shard "shard-1" has voters on node-1 and node-2
    And node-1 has small_file_budget = 50 GB
    And node-2 has small_file_budget = 40 GB
    And shard "shard-1" has an estimated 10,000,000 files
    Then the raw threshold = 40 GB / 10,000,000 = 4096 bytes
    And the shard inline threshold is clamped to 4096 bytes

  Scenario: Threshold clamped to floor when budget is tiny
    Given shard "shard-2" has voters on node-1 and node-2
    And node-1 has small_file_budget = 1 GB
    And shard "shard-2" has an estimated 100,000,000 files
    Then the raw threshold = 1 GB / 100,000,000 = 10 bytes
    And the shard inline threshold is clamped to 128 bytes (floor)

  Scenario: Threshold clamped to ceiling
    Given shard "shard-3" has voters on node-1 and node-2
    And both nodes have small_file_budget = 100 GB
    And shard "shard-3" has an estimated 100 files
    Then the raw threshold = 100 GB / 100 = 1 GB
    And the shard inline threshold is clamped to 65536 bytes (ceiling)

  Scenario: Threshold decrease is automatic and prospective (I-L9)
    Given shard "shard-1" has inline threshold = 4096 bytes
    And 500 files were written with inline data (each < 4096 bytes)
    When node-2's metadata usage approaches soft limit
    And the leader recomputes threshold to 1024 bytes
    Then new files smaller than 1024 bytes are stored inline
    And new files between 1024 and 4096 bytes go to chunk store
    And the 500 existing inline files remain in small/objects.redb
    And no retroactive migration occurs

  Scenario: Threshold increase requires admin action
    Given shard "shard-1" has inline threshold = 128 bytes (floor)
    When the control plane attempts to increase threshold to 4096
    Then the change is rejected without cluster admin approval
    When the cluster admin approves the increase via maintenance mode
    Then the shard inline threshold is set to 4096 bytes
    And a maintenance task is optionally queued to migrate eligible chunked files to inline

  # === Small-file data path (I-SF5) ===

  Scenario: File below threshold stored inline via Raft
    Given shard "shard-1" has inline threshold = 4096 bytes
    When a client writes a 512-byte file
    Then the gateway encrypts the file with envelope encryption
    And the encrypted payload is included in the Raft log entry as delta payload
    And the log entry is replicated to all voters
    And on apply the state machine offloads the payload to small/objects.redb
    And the in-memory state machine retains only the delta header

  Scenario: File above threshold stored as chunk extent
    Given shard "shard-1" has inline threshold = 4096 bytes
    When a client writes a 100 KB file
    Then the gateway encrypts the file
    And a chunk extent is allocated on a raw block device
    And the encrypted data is written via O_DIRECT
    And the delta contains only the chunk_ref (no payload)
    And the Raft log entry carries metadata only

  Scenario: Read path is transparent — checks redb first, then block
    Given shard "shard-1" has both inline and chunked files
    When a client reads an inline file (chunk_id = "abc123")
    Then ChunkOps::get() finds it in small/objects.redb
    And returns the encrypted content
    When a client reads a chunked file (chunk_id = "def456")
    Then ChunkOps::get() misses in small/objects.redb
    And reads from the block device extent
    And returns the encrypted content

  Scenario: Snapshot includes inline content (I-SF5)
    Given shard "shard-1" has 1000 inline files in small/objects.redb
    When the state machine builds a snapshot
    Then the snapshot includes all 1000 inline file contents read from redb
    When a new learner receives this snapshot
    And installs it via install_snapshot
    Then the learner's small/objects.redb contains all 1000 files
    And reads for those files succeed on the learner

  # === Raft throughput guard (I-SF7) ===

  Scenario: Inline write rate exceeds budget — threshold drops to floor
    Given shard "shard-1" has inline threshold = 4096 bytes
    And KISEKI_RAFT_INLINE_MBPS = 10
    When 5000 inline writes of 3000 bytes each arrive in 1 second
    Then the measured inline rate is 15 MB/s (exceeds 10 MB/s budget)
    And the effective threshold drops to 128 bytes (floor)
    And new 3000-byte files are routed to the chunk store
    When the write burst subsides and rate drops below 10 MB/s
    Then the effective threshold returns to 4096 bytes

  # === Metadata capacity management (I-SF2) ===

  Scenario: Soft limit triggers threshold reduction
    Given node-1's metadata usage is at 49% of root disk
    And shard "shard-1" is hosted on node-1 with threshold = 4096
    When metadata usage crosses 50% (soft limit)
    Then node-1 reports capacity pressure via gRPC health report
    And the shard leader recomputes threshold
    And threshold decreases (e.g., to 2048 bytes)

  Scenario: Hard limit forces threshold to floor and emits alert
    Given node-2's metadata usage is at 74%
    When metadata usage crosses 75% (hard limit)
    Then node-2 reports hard-limit breach via gRPC out-of-band
    And the shard leader sets threshold = 128 bytes for all shards on node-2
    And an alert is emitted to cluster admin
    And the leader commits the threshold change via Raft with 2/3 majority

  Scenario: Emergency signal uses gRPC, not Raft
    Given node-2's disk is at 76% (above hard limit)
    And node-2 cannot write new Raft log entries (disk full for journal)
    When node-2 sends capacity report via data-path gRPC channel
    Then the shard leader receives the report
    And commits threshold reduction using votes from node-1 and node-3
    And node-2 receives the committed change via Raft replication

  # === GC for small/objects.redb (I-SF6) ===

  Scenario: Inline file deletion cleans small/objects.redb
    Given an inline file with chunk_id "abc123" exists in small/objects.redb
    When the file is deleted (tombstone delta committed via Raft)
    And all consumer watermarks advance past the tombstone
    And truncate_log or compact_shard runs
    Then the entry for "abc123" is removed from small/objects.redb
    And no orphan entry remains

  Scenario: Orphan detection in small/objects.redb
    Given small/objects.redb has 10,000 entries
    And the delta log references only 9,990 of them
    When a scrub or consistency check runs
    Then 10 orphan entries are detected
    And an alert is emitted for investigation

  # === Workload-driven shard placement ===

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

  Scenario: Homogeneous cluster — only threshold and split available
    Given all 3 nodes have identical hardware (NVMe root + HDD data)
    When shard "shard-1" metadata pressure exceeds soft limit
    Then the control plane lowers the inline threshold
    When the threshold is already at floor
    And shard exceeds the I-L6 split ceiling
    Then the shard is split
    When the shard does not exceed split ceiling
    Then an alert is emitted: "metadata tier at capacity, no placement options"

  Scenario: Migration has zero downtime
    Given shard "shard-1" is receiving writes at 1000 ops/sec
    When a migration from node-1 to node-3 is in progress
    Then writes continue on the current leader without interruption
    And reads continue from existing voters
    And the new voter (node-3) becomes available after catch-up

  Scenario: Failed migration is rolled back
    Given a migration of shard "shard-1" to node-3 is initiated
    And node-3 crashes during learner catch-up
    Then the learner is removed from the Raft group
    And no membership change occurs
    And shard "shard-1" continues operating on original voters

  # === Placement rate limiting (I-SF4) ===

  Scenario: Exponential backoff prevents excessive migrations
    Given shard "shard-1" was migrated at T=0
    Then the next migration for shard "shard-1" is blocked until T+2h
    When it is migrated again at T+2h
    Then the next migration is blocked until T+2h+4h = T+6h
    When it is migrated again at T+6h
    Then the next migration is blocked until T+6h+8h = T+14h
    And the backoff continues doubling up to 24h cap

  Scenario: Backoff reset never goes below 2-hour floor
    Given shard "shard-1" has a backoff of 8 hours
    When the workload profile changes significantly (small_file_ratio crosses 0.3 to 0.8)
    Then the backoff resets to 2 hours (floor), not 30 minutes
    And the shard may be migrated after the 2-hour window

  Scenario: Cluster-wide migration rate limit
    Given a 30-node cluster (max concurrent = max(1, 30/10) = 3)
    When 5 shards are candidates for migration simultaneously
    Then only 3 migrations proceed concurrently
    And the remaining 2 wait until a slot is available

  # === SSD learners as read accelerators ===

  Scenario: Add SSD learner for read-heavy shard
    Given shard "shard-1" has RF=3 voters on HDD nodes
    And shard "shard-1" has high read IOPS for small files
    When an SSD learner is added to shard "shard-1"
    Then the learner receives the full Raft log
    And its small/objects.redb is populated via log replay
    And read requests can be served from the SSD learner
    And the learner does NOT participate in elections
    And the learner does NOT count toward commit quorum

  Scenario: Learner promoted to voter when workload persists
    Given shard "shard-1" has an SSD learner serving reads for 24 hours
    And the small-file workload persists
    When the control plane promotes the SSD learner to voter
    And demotes an HDD voter
    Then the shard has an SSD voter and improved write latency
    And the old HDD voter's data is eventually GC'd

  # === Bimodal read latency (documented behavior) ===

  Scenario: Same-sized files have different latency after threshold drop
    Given shard "shard-1" had threshold = 4096 and stored 1000 files of 2KB inline
    When threshold drops to 128 bytes
    And 1000 new files of 2KB are written (now chunked)
    Then reading old 2KB files returns data from NVMe (< 100us)
    And reading new 2KB files returns data from HDD (5-10ms)
    And this bimodal latency is expected behavior per ADR-030
