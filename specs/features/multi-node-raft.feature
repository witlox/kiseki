Feature: Multi-node Raft — replication, failover, and consistency (ADR-026)

  Raft-per-shard with 3 replicas. Metadata (deltas) replicated via Raft.
  Chunk data uses EC directly. Leader election on failure.

  Background:
    Given a Kiseki cluster with 3 storage nodes [node-1, node-2, node-3]
    And shard "s1" has Raft group on [node-1 (leader), node-2, node-3]

  # === Replication ===

  @library @slow
  Scenario: Delta replicated to majority before ack (I-L2)
    When a client writes a delta to shard "s1" via node-1 (leader)
    Then the delta is written to node-1's local log
    And replicated to at least one follower (node-2 or node-3)
    And the client receives ack only after majority commit

  @library @slow
  Scenario: Read after write — consistent on leader
    When a client writes delta with payload "test" to shard "s1"
    And immediately reads from shard "s1" on node-1 (leader)
    Then the delta with payload "test" is returned

  @library @slow
  Scenario: Follower read may be stale (eventual)
    When a client writes delta to shard "s1" via leader node-1
    And reads from follower node-2 before replication completes
    Then the read may not include the latest delta
    # Note: reads go through leader by default. Follower reads are opt-in.

  # === Leader election ===

  @library @slow
  Scenario: Leader failure triggers election (F-C1)
    When node-1 (leader of shard "s1") becomes unreachable
    Then an election begins among node-2 and node-3
    And a new leader is elected within 300-600ms
    And writes to shard "s1" resume on the new leader

  @library @slow
  Scenario: Election does not lose committed deltas
    Given 100 deltas committed to shard "s1"
    When the leader fails and a new leader is elected
    Then all 100 committed deltas are present on the new leader
    And the sequence numbers are continuous (I-L1)

  @library @slow
  Scenario: Concurrent elections across shards — bounded storm
    Given node-1 hosts leader for 30 shards
    When node-1 fails
    Then 30 elections start with randomized timeouts (150-300ms jitter)
    And all elections complete within 2 seconds
    And no two elections on the same shard overlap

  # === Quorum ===

  @library @slow
  Scenario: Quorum loss blocks writes (F-C2)
    Given shard "s1" has 3 members [node-1, node-2, node-3]
    When node-2 and node-3 both become unreachable
    Then writes to shard "s1" fail with QuorumLost error
    And reads from node-1 (old leader) may still succeed (stale)

  @library @slow
  Scenario: Quorum restored — writes resume
    Given shard "s1" has lost quorum (only node-1 reachable)
    When node-2 comes back online
    Then quorum is restored (2 of 3)
    And writes to shard "s1" resume
    And node-2 catches up via log replay

  # === Member management ===

  @library @slow
  Scenario: Add replica to shard
    Given shard "s1" has 3 members
    When a new node-4 is added as a member
    Then node-4 receives a snapshot of the current state
    And begins receiving new log entries
    And shard "s1" now has 4 members

  @library @slow
  Scenario: Remove replica from shard
    Given shard "s1" has 4 members
    When node-4 is removed from the group
    Then node-4 stops receiving log entries
    And shard "s1" returns to 3 members
    And quorum requirement adjusts accordingly

  # === Network transport ===

  @library @slow
  Scenario: Raft messages travel over TLS
    When node-1 sends a heartbeat to node-2
    Then the message is TLS-encrypted
    And the receiver validates the sender's certificate

  @library @slow
  Scenario: Network partition — minority side cannot elect
    Given nodes [node-1, node-2] are partitioned from [node-3]
    Then [node-1, node-2] form majority and elect a leader
    And [node-3] cannot form quorum alone
    And [node-3] accepts no writes

  # === Snapshot and recovery ===

  @library @slow
  Scenario: New member catches up via snapshot
    Given shard "s1" has 100,000 committed entries
    When a new node-4 joins the group
    Then node-4 receives a snapshot (not 100k individual entries)
    And the snapshot contains the full state machine state
    And node-4 begins receiving new entries from the snapshot point

  @library @slow
  Scenario: Crashed node recovers from local log + network
    Given node-2 crashes with 50,000 entries committed
    When node-2 restarts
    Then it loads its local redb log (entries it already had)
    And receives missing entries from the leader
    And catches up without needing a full snapshot

  # === Placement ===

  @library @slow
  Scenario: Shard members placed on distinct nodes
    When a shard is created with replication factor 3
    Then the 3 Raft members are placed on 3 different nodes
    And no two members share the same physical node

  @library @slow
  Scenario: Rack-aware placement (if configured)
    Given rack-awareness is enabled
    When a shard is created with replication factor 3
    Then the 3 members are placed in at least 2 different racks

  # === Shard migration via membership change (ADR-030) ===

  @library @slow
  Scenario: Shard migrated to SSD node via learner promotion
    Given shard "s1" has voters on [node-1, node-2, node-3] (all HDD)
    And node-4 is an SSD node with available capacity
    When the control plane initiates migration of "s1" to node-4
    Then node-4 is added as a learner
    And node-4 receives a snapshot and catches up
    And node-4 is promoted to voter
    And one HDD node is removed from the voter set
    And writes continue throughout without interruption

  @library @slow
  Scenario: Learner added as read accelerator (ADR-030 §7)
    Given shard "s1" has voters on [node-1, node-2, node-3]
    When an SSD learner is added on node-4
    Then node-4 receives the Raft log but does not vote
    And node-4 can serve read requests
    And removing node-4 does not affect write quorum

  # === Node lifecycle / drain (I-N1..I-N7 — ADR-035, spec-only) ===

  @library @slow
  Scenario: Operator drains a node — leadership transfers off
    Given the cluster has 4 Active nodes [node-1, node-2, node-3, node-4]
    And node-1 leads shards "s1" and "s2"
    And node-1 holds voter slots in shards "s1", "s2", "s3"
    When the cluster admin issues `DrainNode(node-1)`
    Then node-1's state transitions Active → Draining
    And leadership for "s1" is transferred to a voter on another node (node-2 or node-3 per I-L12)
    And leadership for "s2" is similarly transferred
    And node-1 holds zero leader assignments

  @library @slow
  Scenario: Drain completes with full re-replication (I-N3, I-N5)
    Given node-1 is Draining and has been stripped of leadership
    And node-1 still holds voter slots in shards "s1", "s2", "s3"
    When the drain orchestrator runs voter replacement for each affected shard
    Then for each shard, a learner is added on a surviving node and caught up to the leader's committed index
    And the learner is promoted to voter
    And node-1 is removed from the voter set
    And RF=3 is preserved at every intermediate state — no shard observes RF<3 during the drain
    And once all three shards have completed voter replacement, node-1 transitions Draining → Evicted

  @library @slow
  Scenario: Drain refused at RF floor (I-N4)
    Given the cluster has exactly 3 Active nodes [node-1, node-2, node-3]
    And every shard has voters on all 3 nodes (RF=3)
    When the cluster admin issues `DrainNode(node-1)` without first adding a replacement
    Then the request is rejected with "DrainRefused: insufficient capacity to maintain RF=3"
    And node-1 remains in state Active
    And no leadership transfer or voter replacement is attempted
    And the refusal is recorded in the cluster audit shard (I-N6)

  @library @slow
  Scenario: Drain proceeds after replacement node is added (I-N4 mitigation)
    Given the cluster has 3 Active nodes and a previous DrainRefused for node-1
    When the cluster admin adds node-4 (now 4 Active nodes)
    And the cluster admin re-issues `DrainNode(node-1)`
    Then the drain is accepted
    And voter replacements target node-4 first by best-effort placement
    And the drain completes per the standard protocol

  @library @slow
  Scenario: Drain cancellation returns node to Active (I-N7)
    Given node-1 is in state Draining
    And voter replacement has completed for "s1" but not yet for "s2" or "s3"
    When the cluster admin issues `CancelDrain(node-1)`
    Then node-1 transitions Draining → Active (the only permitted reverse transition)
    And pending voter replacements for "s2" and "s3" are aborted
    And the completed voter replacement for "s1" is NOT rolled back — node-1 is no longer in "s1"'s voter set
    And the cluster operates correctly with the resulting placement
    And the cancellation is recorded in the cluster audit shard

  @library @slow
  Scenario: Drain concurrency bounded by I-SF4 cap
    Given node-1 is Draining with voter slots in 100 shards
    When the drain orchestrator schedules voter replacements
    Then no more than `max(1, num_nodes / 10)` replacements are in flight simultaneously
    And remaining replacements are queued
    And the drain completes in bounded time without Raft instability

  @library @slow
  Scenario: Evicted state is terminal (I-N1)
    Given node-1 is in state Evicted
    When the cluster admin attempts to re-activate node-1
    Then the request is rejected with "node identity is Evicted; re-add requires fresh node identity"
    And node-1 remains in state Evicted

  @library @slow
  Scenario: Split fires during active drain — leader not placed on draining node (ADV-033-8)
    Given node-1 is in state Draining
    And shard "s5" exceeds its hard ceiling (I-L6)
    When the auto-split trigger fires for "s5"
    Then a new shard "s5-b" is created
    And "s5-b"'s leader is placed on a node in {Active, Degraded} state — NOT on node-1
    And the I-L12 placement engine excludes Failed, Draining, and Evicted nodes

  @library @slow
  Scenario: Degraded node is eligible as drain replacement target (ADV-035-10)
    Given the cluster has 4 nodes: node-1 (Active), node-2 (Active), node-3 (Degraded), node-4 (Active)
    And node-4 holds voter slots in shards "s1", "s2", "s3"
    When the cluster admin issues `DrainNode(node-4)`
    Then node-3 (Degraded) is eligible as a replacement voter target
    And voter replacements may be placed on node-3
    And the drain completes successfully

  @library @slow
  Scenario: Failed node recovers after eviction — stale membership harmless (ADV-035-5)
    Given node-1 was Failed and then drained to Evicted
    When node-1 physically recovers and its Raft instances restart
    Then node-1 receives AppendEntries with a higher term showing its removal
    And node-1 steps down and does not rejoin any voter set
    And the control plane NodeRecord for node-1 remains Evicted

  # === Performance ===

  @library @slow
  Scenario: Write latency within SLO
    When 1000 sequential delta writes are performed
    Then the p99 write latency is under 500µs (TCP) or 100µs (RDMA)

  @library @slow
  Scenario: Throughput scales with shard count
    Given 10 shards on 3 nodes
    When all 10 shards receive concurrent writes
    Then total throughput is approximately 10x single-shard throughput
    And per-shard throughput is not degraded by other shards

  # === Cross-node chunk replication (Phase 16a) ===
  #
  # These scenarios verify the ClusterChunkService fabric layer that
  # makes a 3-node cluster genuinely tolerant of single-node loss.
  # See specs/implementation/phase-16-cross-node-chunks.md (rev 4)
  # and ADR-026 for the design rationale (D-1, D-5, D-6, D-7, D-10).

  @library @cross-node
  Scenario: Cross-node read after leader-only PUT (closes B-3)
    Given a 3-node Replication-3 cluster on [node-1, node-2, node-3]
    When a client PUTs object "obj-x1" via node-1's S3 listener
    Then the chunk's fragment lands on node-1's local store
    And `PutFragment` for that chunk lands on node-2 within 5 seconds
    And `PutFragment` for that chunk lands on node-3 within 5 seconds
    And a subsequent S3 GET of "obj-x1" via node-2 returns the same bytes
    And the GET on node-2 served the chunk from its local store, not via fabric

  @library @cross-node
  Scenario: Read survives single-node failure (D-1 the whole point)
    Given a 3-node Replication-3 cluster with composition "obj-x2" stored
    And every node has acked `PutFragment` for the chunks of "obj-x2"
    When node-1 is killed
    Then a client's S3 GET of "obj-x2" via node-2 still returns the bytes
    And the GET completes within 5 seconds
    And no NFS4ERR_DELAY or 503 is surfaced — the read is degraded, not failed

  @library @cross-node
  Scenario: Write requires 2-of-3 quorum (D-5)
    Given a 3-node Replication-3 cluster on [node-1, node-2, node-3]
    And `min_acks = 2` (default for Replication-3)
    When node-2 and node-3 are both unreachable from node-1
    And a client attempts a PUT via node-1
    Then node-1's local write succeeds (1 ack)
    And `PutFragment` to node-2 fails within the 5s per-peer timeout
    And `PutFragment` to node-3 fails within the 5s per-peer timeout
    And the client receives 503 with retry-after metadata
    And `kiseki_fabric_quorum_lost_total` increments by 1

  @library @cross-node @ordering
  Scenario: Composition delta arrives before fragment (D-10 cross-stream)
    Given a 3-node Replication-3 cluster with a slow node-3
    And node-3 applies Raft entries 50% faster than it acknowledges fabric `PutFragment`
    When a PUT lands the composition delta on node-3 before the fragment
    And a client's S3 GET via node-3 references the new chunk
    Then node-3's read finds the chunk absent locally
    And node-3 falls back to `GetFragment` against node-1 or node-2
    And the read returns the same bytes as the original PUT

  @library @cross-node @leader-change
  Scenario: Refcount preserved across leader change (D-4 / cluster_chunk_state)
    Given a 3-node Replication-3 cluster with composition "obj-x3" (refcount=1)
    And every replica has applied the `ChunkAndDelta` proposal that created "obj-x3"
    When the leader is killed and a new leader is elected
    Then the new leader's `cluster_chunk_state[("default", chunk_of(obj-x3))].refcount` is 1
    And `kiseki-control inspect-chunk` for that chunk reports refcount=1

  @library @cross-node
  Scenario: Tenant cert presented to fabric port is rejected (I-Auth4)
    Given a 3-node Replication-3 cluster with mTLS enabled
    And the data-path cert carries `spiffe://cluster/fabric/<node-id>` SAN URIs
    When a tenant client presents a `spiffe://cluster/org/<uuid>` cert
    And calls `PutFragment` directly against node-1's data-path port
    Then the SAN-role interceptor rejects the call with PermissionDenied
    And the local chunk store is unmodified
    And no fragment fan-out occurs

  @library @cross-node @degenerate
  Scenario: 1-node cluster degenerates to local-only (D-6)
    Given a single-node cluster with empty `raft_peers`
    When a client PUTs object "obj-y1" via the node's S3 listener
    Then the chunk lands on the local store
    And `min_acks` is capped at the replication factor (1)
    And no fan-out RPCs are issued (peer list is empty)
    And the client receives 200 OK without QuorumLost
