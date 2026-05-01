Feature: Persistence and crash recovery (ADR-022)

  Data survives server restart. Raft log persisted via redb.
  Chunk data in pool files. View watermarks checkpointed.

  Background:
    Given a Kiseki server with KISEKI_DATA_DIR configured
    And redb database at $DATA_DIR/raft/db.redb
    And pool files at $DATA_DIR/pools/

  # === Raft log persistence ===

  @library @slow
  Scenario: Delta survives server restart
    Given a delta was written via LogService AppendDelta
    When the server is restarted
    Then the delta is readable via ReadDeltas
    And the sequence number is preserved

  @library @slow
  Scenario: Multiple deltas survive restart
    Given 100 deltas were written to shard "s1"
    When the server is restarted
    Then all 100 deltas are readable
    And their order is preserved (I-L1)

  @library @slow
  Scenario: Raft vote and term survive restart
    Given the Raft group elected leader at term 5
    When the server is restarted
    Then the persisted term is 5
    And the vote is preserved

  # === Raft snapshots ===

  @library @slow
  Scenario: Snapshot taken after 10,000 entries
    Given 10,000 deltas have been written since last snapshot
    Then a snapshot is automatically created
    And the snapshot contains the full state machine state
    And log entries before the snapshot can be truncated

  @library @slow
  Scenario: Restore from snapshot + replay
    Given a snapshot exists at log index 10,000
    And 500 additional log entries exist (10,001 to 10,500)
    When the server is restarted
    Then the state machine is restored from the snapshot
    And entries 10,001-10,500 are replayed
    And the final state matches pre-restart state

  @library @slow
  Scenario: Snapshot survives restart
    Given a snapshot was taken
    When the server is restarted
    Then the snapshot is still available in redb
    And new entries can be appended after the snapshot

  # === Chunk data persistence ===

  @library @slow
  Scenario: Chunk data survives restart
    Given a chunk was written via the gateway (encrypt + store)
    When the server is restarted
    Then the chunk is readable via the gateway (decrypt + return)
    And the plaintext matches the original

  @library @slow
  Scenario: Pool file integrity
    Given 1000 chunks stored in pool file "fast-nvme-dev0.pool"
    When the server is restarted
    Then all 1000 chunks are accessible
    And their offsets in the pool file are correct

  # === View watermarks ===

  @library @slow
  Scenario: View watermark survives restart
    Given the stream processor advanced view "v1" to watermark 500
    When the server is restarted
    Then view "v1" watermark is restored to 500
    And the stream processor resumes from watermark 501

  # === Key manager persistence ===

  @library @slow
  Scenario: Key epochs survive restart
    Given the key manager has epochs [1, 2, 3] with epoch 3 current
    When the server is restarted
    Then all three epochs are available
    And the current epoch is still 3

  # === Small-file inline content persistence (ADR-030) ===

  @library @slow
  Scenario: Inline small files survive restart
    Given 100 files below the inline threshold were written
    And their content is in small/objects.redb
    When the server is restarted
    Then all 100 files are readable from small/objects.redb
    And their encrypted content matches the original writes

  @library @slow
  Scenario: Inline files included in Raft snapshot
    Given shard "s1" has 500 inline files in small/objects.redb
    When a Raft snapshot is built for shard "s1"
    Then the snapshot data includes all 500 inline file contents
    When a new node installs this snapshot
    Then its small/objects.redb contains all 500 entries

  # === Crash recovery edge cases ===

  @library @slow
  Scenario: Crash during write — partial data not visible
    Given a delta write is in progress (Raft not yet committed)
    When the server crashes
    And the server is restarted
    Then the uncommitted delta is not visible
    And the log is consistent (no partial entries)

  @library @slow
  Scenario: Crash during snapshot — old snapshot preserved
    Given a snapshot is being written
    When the server crashes mid-snapshot
    And the server is restarted
    Then the previous valid snapshot is used
    And no corrupted snapshot data is loaded
