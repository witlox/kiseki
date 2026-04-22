Feature: Multi-node Raft — replication, failover, and consistency (ADR-026)

  Raft-per-shard with 3 replicas. Metadata (deltas) replicated via Raft.
  Chunk data uses EC directly. Leader election on failure.

  Background:
    Given a Kiseki cluster with 3 storage nodes [node-1, node-2, node-3]
    And shard "s1" has Raft group on [node-1 (leader), node-2, node-3]

  # === Replication ===

  Scenario: Delta replicated to majority before ack (I-L2)
    When a client writes a delta to shard "s1" via node-1 (leader)
    Then the delta is written to node-1's local log
    And replicated to at least one follower (node-2 or node-3)
    And the client receives ack only after majority commit

  Scenario: Read after write — consistent on leader
    When a client writes delta with payload "test" to shard "s1"
    And immediately reads from shard "s1" on node-1 (leader)
    Then the delta with payload "test" is returned

  Scenario: Follower read may be stale (eventual)
    When a client writes delta to shard "s1" via leader node-1
    And reads from follower node-2 before replication completes
    Then the read may not include the latest delta
    # Note: reads go through leader by default. Follower reads are opt-in.

  # === Leader election ===

  Scenario: Leader failure triggers election (F-C1)
    When node-1 (leader of shard "s1") becomes unreachable
    Then an election begins among node-2 and node-3
    And a new leader is elected within 300-600ms
    And writes to shard "s1" resume on the new leader

  Scenario: Election does not lose committed deltas
    Given 100 deltas committed to shard "s1"
    When the leader fails and a new leader is elected
    Then all 100 committed deltas are present on the new leader
    And the sequence numbers are continuous (I-L1)

  Scenario: Concurrent elections across shards — bounded storm
    Given node-1 hosts leader for 30 shards
    When node-1 fails
    Then 30 elections start with randomized timeouts (150-300ms jitter)
    And all elections complete within 2 seconds
    And no two elections on the same shard overlap

  # === Quorum ===

  Scenario: Quorum loss blocks writes (F-C2)
    Given shard "s1" has 3 members [node-1, node-2, node-3]
    When node-2 and node-3 both become unreachable
    Then writes to shard "s1" fail with QuorumLost error
    And reads from node-1 (old leader) may still succeed (stale)

  Scenario: Quorum restored — writes resume
    Given shard "s1" has lost quorum (only node-1 reachable)
    When node-2 comes back online
    Then quorum is restored (2 of 3)
    And writes to shard "s1" resume
    And node-2 catches up via log replay

  # === Member management ===

  Scenario: Add replica to shard
    Given shard "s1" has 3 members
    When a new node-4 is added as a member
    Then node-4 receives a snapshot of the current state
    And begins receiving new log entries
    And shard "s1" now has 4 members

  Scenario: Remove replica from shard
    Given shard "s1" has 4 members
    When node-4 is removed from the group
    Then node-4 stops receiving log entries
    And shard "s1" returns to 3 members
    And quorum requirement adjusts accordingly

  # === Network transport ===

  Scenario: Raft messages travel over TLS
    When node-1 sends a heartbeat to node-2
    Then the message is TLS-encrypted
    And the receiver validates the sender's certificate

  Scenario: Network partition — minority side cannot elect
    Given nodes [node-1, node-2] are partitioned from [node-3]
    Then [node-1, node-2] form majority and elect a leader
    And [node-3] cannot form quorum alone
    And [node-3] accepts no writes

  # === Snapshot and recovery ===

  Scenario: New member catches up via snapshot
    Given shard "s1" has 100,000 committed entries
    When a new node-4 joins the group
    Then node-4 receives a snapshot (not 100k individual entries)
    And the snapshot contains the full state machine state
    And node-4 begins receiving new entries from the snapshot point

  Scenario: Crashed node recovers from local log + network
    Given node-2 crashes with 50,000 entries committed
    When node-2 restarts
    Then it loads its local redb log (entries it already had)
    And receives missing entries from the leader
    And catches up without needing a full snapshot

  # === Placement ===

  Scenario: Shard members placed on distinct nodes
    When a shard is created with replication factor 3
    Then the 3 Raft members are placed on 3 different nodes
    And no two members share the same physical node

  Scenario: Rack-aware placement (if configured)
    Given rack-awareness is enabled
    When a shard is created with replication factor 3
    Then the 3 members are placed in at least 2 different racks

  # === Shard migration via membership change (ADR-030) ===

  Scenario: Shard migrated to SSD node via learner promotion
    Given shard "s1" has voters on [node-1, node-2, node-3] (all HDD)
    And node-4 is an SSD node with available capacity
    When the control plane initiates migration of "s1" to node-4
    Then node-4 is added as a learner
    And node-4 receives a snapshot and catches up
    And node-4 is promoted to voter
    And one HDD node is removed from the voter set
    And writes continue throughout without interruption

  Scenario: Learner added as read accelerator (ADR-030 §7)
    Given shard "s1" has voters on [node-1, node-2, node-3]
    When an SSD learner is added on node-4
    Then node-4 receives the Raft log but does not vote
    And node-4 can serve read requests
    And removing node-4 does not affect write quorum

  # === Performance ===

  Scenario: Write latency within SLO
    When 1000 sequential delta writes are performed
    Then the p99 write latency is under 500µs (TCP) or 100µs (RDMA)

  Scenario: Throughput scales with shard count
    Given 10 shards on 3 nodes
    When all 10 shards receive concurrent writes
    Then total throughput is approximately 10x single-shard throughput
    And per-shard throughput is not degraded by other shards
