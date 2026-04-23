Feature: Cluster formation — multi-node Raft group bootstrap and join

  Nodes form a Raft cluster for a shard. One node seeds the cluster,
  others join by receiving membership from the seed's Raft leader.
  This ensures correct cluster formation regardless of startup order.

  Background:
    Given 3 Raft-capable nodes with TCP transport

  # === Seed bootstrap ===

  Scenario: Seed node initializes and becomes leader
    When node-1 creates a shard as seed with 3 members [1, 2, 3]
    Then node-1 calls raft.initialize() with all 3 members
    And node-1 becomes leader (single-node quorum until peers join)
    And node-1 accepts writes immediately

  Scenario: Seed node starts RPC server before other nodes join
    When node-1 creates a shard as seed
    Then node-1's Raft RPC server is listening
    And node-1 can accept incoming Vote and AppendEntries RPCs

  # === Follower join ===

  Scenario: Follower joins existing cluster without calling initialize
    Given node-1 has seeded the cluster and is leader
    When node-2 creates its Raft instance for the same shard
    Then node-2 does NOT call raft.initialize()
    And node-2 starts its RPC server
    And node-2 receives membership from node-1 via AppendEntries
    And node-2 becomes a follower

  Scenario: Follower joins even if seed started minutes earlier
    Given node-1 has been running as leader for 60 seconds
    When node-2 starts and joins the cluster
    Then node-2 successfully becomes a follower
    And node-2 receives any committed log entries from the leader

  Scenario: All 3 nodes form a healthy cluster
    Given node-1 has seeded the cluster
    When node-2 and node-3 join the cluster
    Then all 3 nodes are part of the Raft membership
    And the cluster has a single leader
    And writes through the leader are replicated to followers
    And reads from any node return committed data

  # === Staggered startup ===

  Scenario: Nodes can join in any order after seed
    Given node-1 has seeded the cluster
    When node-3 joins before node-2
    Then node-3 becomes a follower
    And when node-2 joins later, it also becomes a follower
    And the cluster has 3 healthy members

  Scenario: Cluster reaches quorum when majority joins
    Given node-1 has seeded the cluster (1 of 3 — no quorum)
    When node-2 joins (2 of 3 — quorum reached)
    Then the leader can commit writes (majority = 2)
    And node-3 can join later without disrupting the cluster

  # === Leader election after formation ===

  Scenario: Leader election works after cluster formation
    Given a 3-node cluster is fully formed
    When the leader's Raft RPC server stops
    Then a new leader is elected from the remaining 2 nodes
    And writes continue on the new leader

  # === Configuration ===

  Scenario: Seed vs follower determined by bootstrap flag
    Given KISEKI_BOOTSTRAP=true on node-1
    And KISEKI_BOOTSTRAP=false on node-2 and node-3
    When all 3 nodes start
    Then only node-1 calls raft.initialize()
    And node-2 and node-3 wait for membership from the leader

  # === Error handling ===

  Scenario: Follower retries if seed is not yet available
    When node-2 starts before node-1 (seed)
    Then node-2's RPC server starts and listens
    And node-2 retries connecting to the seed
    And once node-1 starts, node-2 receives membership and joins

  Scenario: Double initialize is harmless on the same node
    When node-1 calls initialize() twice with the same membership
    Then the second call is a no-op (idempotent)
    And the cluster continues operating normally
