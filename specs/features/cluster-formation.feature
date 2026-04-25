Feature: Cluster formation — multi-node Raft group bootstrap and join

  Nodes form a Raft cluster for a shard. One node seeds the cluster,
  others join by receiving membership from the seed's Raft leader.
  This ensures correct cluster formation regardless of startup order.

  Background:
    Given 3 Raft-capable nodes with TCP transport

  # === Seed bootstrap ===

  @integration
  Scenario: Seed node initializes and becomes leader
    When node-1 creates a shard as seed with 3 members [1, 2, 3]
    Then node-1 calls raft.initialize() with all 3 members
    And node-1 becomes leader (single-node quorum until peers join)
    And node-1 accepts writes immediately

  @integration
  Scenario: Seed node starts RPC server before other nodes join
    When node-1 creates a shard as seed
    Then node-1's Raft RPC server is listening
    And node-1 can accept incoming Vote and AppendEntries RPCs

  # === Follower join ===

  @integration
  Scenario: Follower joins existing cluster without calling initialize
    Given node-1 has seeded the cluster and is leader
    When node-2 creates its Raft instance for the same shard
    Then node-2 does NOT call raft.initialize()
    And node-2 starts its RPC server
    And node-2 receives membership from node-1 via AppendEntries
    And node-2 becomes a follower

  @integration
  Scenario: Follower joins even if seed started minutes earlier
    Given node-1 has been running as leader for 60 seconds
    When node-2 starts and joins the cluster
    Then node-2 successfully becomes a follower
    And node-2 receives any committed log entries from the leader

  @integration
  Scenario: All 3 nodes form a healthy cluster
    Given node-1 has seeded the cluster
    When node-2 and node-3 join the cluster
    Then all 3 nodes are part of the Raft membership
    And the cluster has a single leader
    And writes through the leader are replicated to followers
    And reads from any node return committed data

  # === Staggered startup ===

  @integration
  Scenario: Nodes can join in any order after seed
    Given node-1 has seeded the cluster
    When node-3 joins before node-2
    Then node-3 becomes a follower
    And when node-2 joins later, it also becomes a follower
    And the cluster has 3 healthy members

  @integration
  Scenario: Cluster reaches quorum when majority joins
    Given node-1 has seeded the cluster (1 of 3 — no quorum)
    When node-2 joins (2 of 3 — quorum reached)
    Then the leader can commit writes (majority = 2)
    And node-3 can join later without disrupting the cluster

  # === Leader election after formation ===

  @integration
  Scenario: Leader election works after cluster formation
    Given a 3-node cluster is fully formed
    When the leader's Raft RPC server stops
    Then a new leader is elected from the remaining 2 nodes
    And writes continue on the new leader

  # === Configuration ===

  @integration
  Scenario: Seed vs follower determined by bootstrap flag
    Given KISEKI_BOOTSTRAP=true on node-1
    And KISEKI_BOOTSTRAP=false on node-2 and node-3
    When all 3 nodes start
    Then only node-1 calls raft.initialize()
    And node-2 and node-3 wait for membership from the leader

  # === Error handling ===

  @integration
  Scenario: Follower retries if seed is not yet available
    When node-2 starts before node-1 (seed)
    Then node-2's RPC server starts and listens
    And node-2 retries connecting to the seed
    And once node-1 starts, node-2 receives membership and joins

  @integration
  Scenario: Double initialize is harmless on the same node
    When node-1 calls initialize() twice with the same membership
    Then the second call is a no-op (idempotent)
    And the cluster continues operating normally

  # === Initial shard topology (I-L10, I-L12, I-L15 — ADR-033, spec-only) ===

  @integration
  Scenario: Namespace creation produces 3x node_count shards by default
    Given the cluster has 3 Active nodes
    And no cluster-admin override of `initial_shard_multiplier` is in effect
    And no tenant-admin override for tenant "org-pharma"
    When tenant admin "org-pharma" creates namespace "patient-data"
    Then 9 shards are created for "patient-data"
    And each shard's leader is placed on a distinct node where possible
    And no node hosts more than ceil(9 / 3) = 3 leaders for "patient-data"
    And the namespace shard map records all 9 shards with disjoint hashed_key ranges covering the full key space
    And the namespace shard map is persisted in the control plane Raft group (I-L15)

  @integration
  Scenario: Initial topology floor — small cluster
    Given the cluster has 1 Active node
    When tenant admin creates namespace "small-ns"
    Then 3 shards are created (floor: max(min(3, 64), 3))
    And all 3 leaders are on the single node (best-effort honors what is available)
    And the namespace shard map is persisted

  @integration
  Scenario: Initial topology cap — large cluster
    Given the cluster has 100 Active nodes
    When tenant admin creates namespace "big-ns"
    Then 64 shards are created (cap: min(300, 64))
    And the 64 leaders are placed best-effort round-robin across the 100 nodes
    And approximately 64/100 nodes host one leader; remaining nodes host none for this namespace

  @integration
  Scenario: Cluster admin overrides initial multiplier
    Given the cluster admin sets `initial_shard_multiplier = 2` cluster-wide
    And the cluster has 5 Active nodes
    When tenant admin creates namespace "ns-x"
    Then 10 shards are created (max(min(2 * 5, 64), 3))

  @integration
  Scenario: Tenant admin overrides within admin envelope
    Given the cluster admin defines per-tenant initial-shard bounds: min=4, max=32
    And the cluster has 10 Active nodes
    When tenant admin requests `initial_shards = 16` for namespace "tuned-ns"
    Then 16 shards are created
    But when tenant admin requests `initial_shards = 64`
    Then the request is rejected with "initial_shards exceeds tenant ceiling (32)"

  # === Ratio floor trigger (I-L11 — ADR-033, spec-only) ===

  @integration
  Scenario: Adding a node below the ratio floor triggers auto-split
    Given the cluster has 3 Active nodes
    And namespace "ns-a" has 9 shards (ratio = 3.0)
    When 4 more nodes are added to the cluster (now 7 Active nodes; ratio = 9/7 ≈ 1.29)
    Then the ratio floor is violated (1.29 < 1.5)
    And auto-split fires for "ns-a" until shard count reaches at least ceil(1.5 * 7) = 11
    And the new shards are placed best-effort round-robin so leaders distribute across the 7 nodes
    And the namespace shard map is updated atomically through the control plane Raft group

  @integration
  Scenario: Adding a node within the ratio floor does not trigger split
    Given the cluster has 3 Active nodes
    And namespace "ns-b" has 9 shards (ratio = 3.0)
    When 1 more node is added (now 4 Active nodes; ratio = 9/4 = 2.25)
    Then the ratio floor is satisfied (2.25 >= 1.5)
    And no auto-split is triggered for "ns-b"

  # === Adversary findings (ADV-033) ===

  @integration
  Scenario: Namespace creation is atomic — partial Raft group failure rolls back (ADV-033-1)
    Given the cluster has 3 Active nodes
    And node-3 is temporarily unreachable
    When tenant admin creates namespace "partial-ns" (requires 9 shards)
    And shard 7 fails to reach quorum within 30 seconds (node-3 down)
    Then all 6 successfully created Raft groups are torn down
    And no namespace shard map entry is committed
    And the CreateNamespace call returns error "namespace creation failed: shard 7 did not reach quorum"
    And a subsequent CreateNamespace for "partial-ns" succeeds once node-3 recovers

  @integration
  Scenario: Concurrent CreateNamespace for same ID is rejected during creation (ADV-033-1)
    Given namespace "dup-ns" is in state Creating (Raft groups being formed)
    When a second CreateNamespace("dup-ns") arrives
    Then the second call is rejected with "namespace creation in progress"
    And the first creation continues

  @integration
  Scenario: Write to wrong shard is rejected with KeyOutOfRange (ADV-033-3)
    Given namespace "ns-routed" has 3 shards covering ranges [0x00, 0x55), [0x55, 0xAA), [0xAA, 0xFF]
    And the gateway has a stale shard map (pre-split, single shard)
    When the gateway sends a delta with hashed_key=0x80 to shard-1 (range [0x00, 0x55))
    Then shard-1 rejects the delta with KeyOutOfRange
    And the gateway refreshes its shard map via GetNamespaceShardMap
    And the gateway retries to shard-2 (range [0x55, 0xAA))
    And the delta is accepted

  @integration
  Scenario: Ratio-floor splits respect shard cap (ADV-033-7)
    Given the cluster scales from 3 to 50 Active nodes
    And namespace "big-ns" has 9 shards (ratio = 9/50 = 0.18, far below floor)
    When the ratio-floor evaluator fires
    Then splits fire until shard count reaches min(ceil(1.5 * 50), 64) = 64
    And not 75 (the shard_cap takes precedence)
    And at most max(1, 50/5) = 10 splits are in flight concurrently

  @integration
  Scenario: GetNamespaceShardMap requires tenant authorization (ADV-033-9)
    Given tenant "org-alpha" owns namespace "alpha-data"
    And a gateway authenticated as tenant "org-beta"
    When the gateway calls GetNamespaceShardMap("alpha-data")
    Then the call is rejected with PermissionDenied
    And no shard topology information is returned
