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
  #
  # `@smoke` tag on a curated subset:
  #   * `Cross-node read after leader-only PUT (B-3)` — basic
  #     cross-node read path
  #   * `Read survives leader failure (D-1)` — leader-change
  #   * `Write requires 2-of-3 quorum (D-5)` — quorum loss
  #   * `Tenant cert presented to fabric port is rejected (I-Auth4)`
  #     — mTLS / SAN role rejection
  #   * `Admin SplitShard returns a new shard id` — control-plane
  #     consensus + leader forwarding (ADR-033 §4)
  #
  # CI fast lane (`KISEKI_BDD_FAST=1`) runs only the smoke set from
  # `@integration` scenarios — full suite runs on release / nightly.

  @integration @multi-node @cross-node @smoke
  Scenario: Cross-node read after leader-only PUT (closes B-3)
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    And every follower has received the fragment
    Then S3 GET from node-2 returns the same 1MB
    And the GET on node-2 was served from its local store, not via fabric

  # ADR-040 Phase 18 closure. The 3rd GCP perf run (2026-05-04) hit
  # this gap directly: `PUT /<fresh-bucket>` registered the
  # namespace ONLY on the contacted node, follower hydrators saw
  # the Create delta for an unregistered namespace and skipped in
  # transient retry forever (`reason="namespace_not_registered"`).
  # The scenario above uses the bootstrap "default" bucket where
  # every node pre-registers the namespace at boot, so the symptom
  # never surfaced in tests. This one exercises a fresh bucket
  # where only the new `OperationType::NamespaceCreate` delta makes
  # cross-node visibility work.
  @integration @multi-node @cross-node
  Scenario: Cross-node read after leader-only bucket-CREATE-and-PUT (Phase 18)
    Given a 3-node kiseki cluster
    When a client creates a fresh bucket on node-1
    And a client writes 1MB to that fresh bucket on node-1
    And every follower has received the fragment
    Then S3 GET from any follower in the fresh bucket returns the same 1MB

  # GH #102: on the 6-node EC-4+2 path, the 2026-05-27 GCP run showed
  # reads failing 100% with "AEAD authentication failed" after EC
  # decode. The 3-node cross-node read scenarios above only exercise
  # Replication-3 (whole-envelope, no EC reassembly). This drives the
  # real EC-4+2 cross-node read: PUT on node-1, GET on node-2 (which
  # holds only one fragment, so it must collect + decode + decrypt
  # across the fabric).
  @integration @multi-node @cross-node
  Scenario: 6-node EC-4+2 cross-node read round-trips via S3 (GH #102)
    Given a 6-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    Then S3 GET from node-2 returns the same 1MB

  # GH #102 / ADR-044 convergent-encryption guard (adversary Finding 3):
  # concurrent writes of IDENTICAL content collide on one content-
  # addressed chunk_id and race past the dedup-skip onto the EC fan-out.
  # Pre-fix (random nonce) the fragments tore → AEAD fail on read; the
  # deterministic nonce makes every seal byte-identical so reads stay
  # consistent. S3-only, so not blocked by the native-proxy harness gap
  # (#103).
  @integration @multi-node @cross-node @dedup
  Scenario: 6-node EC-4+2 concurrent identical-content writes dedup and read back (GH #102)
    Given a 6-node kiseki cluster
    When 4 clients concurrently PUT identical 1MB content to distinct keys
    Then S3 GET of each key from node-2 returns the identical 1MB

  # GH #111: distributed multi-shard writes. A `--shards N` namespace
  # spreads shard leaders across nodes; an S3 PUT landing on a node that
  # does NOT lead the target shard must forward the built append to the
  # leader's LogService (the gateway append-forwarder), not 500 with
  # "leader unavailable". All objects are written to node-1 on purpose,
  # so ~5/6 route to remote-led shards and exercise the forward.
  @integration @multi-node @cross-node @forwarding
  Scenario: Distributed multi-shard S3 writes forward to the shard leader (GH #111)
    Given a 6-node kiseki cluster
    And a 6-shard namespace "msfwd" with leaders distributed across the cluster
    When 60 distinct 64KB objects are written via S3 to node-1
    Then every S3 write committed with no errors
    And each object reads back identically from node-2

  # GH #115: when the chunk-store device fills, the gateway must return a
  # clean 507 Insufficient Storage (POSIX ENOSPC shape), not the opaque
  # `device full -> quorum lost 0/N -> 500` chain the perf matrix hit.
  # The cluster's chunk device is capped to 16 MiB so a handful of 1 MiB
  # PUTs fills it. This is the real failure-mode fix end-to-end on a
  # spawned binary — previously only unit + @library-mock coverage.
  @integration @multi-node @capacity @enospc
  Scenario: Chunk pool full returns a clean 507, not a 500 (GH #115)
    Given a single-node cluster with a capped chunk device
    When distinct 1MB S3 objects are PUT until the chunk device is full
    Then at least one PUT returns HTTP 507 Insufficient Storage
    And no PUT returns HTTP 500

  # GH #115: capacity + dedup observability — the per-node storage gauges
  # (used/total/logical/physical) were registered but never set; this
  # asserts they're live end-to-end (gauge -> aggregator -> capacity API)
  # on a real Replication-3 cluster. Identical content must dedup so the
  # cluster-wide logical bytes exceed the physical bytes.
  @integration @multi-node @capacity @dedup
  Scenario: Capacity and dedup are observable on a real cluster (GH #115)
    Given a 3-node kiseki cluster
    When 20 distinct and 20 identical 256KB S3 objects are written for capacity accounting
    Then the cluster capacity report shows non-zero used bytes
    And the cluster dedup ratio exceeds 1.0

  @integration @multi-node @cross-node @smoke
  Scenario: Read survives leader failure (D-1)
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    And every follower has received the fragment
    And the current leader is killed
    Then a new leader is elected within 15 seconds
    And S3 GET from any surviving node returns the same 1MB within 5 seconds
    And the killed node is restarted and rejoins the cluster

  # Promoted to @integration via the test-only fabric-deny knob
  # (POST /admin/test/fabric/deny-incoming/1; gated by
  # KISEKI_ENABLE_TEST_KNOBS=1). Denying a node's PutFragment handler
  # makes it return Unavailable WITHOUT touching Raft — so the cluster
  # still has Raft quorum (3 alive, 3 voters) but fabric acks fall short.
  #
  # Deny the TWO NON-LEADER nodes (discovered at run time), not a fixed
  # node-2 + node-3: Raft election is non-deterministic, and if the leader
  # is itself node-2 or node-3 then denying node-2 + node-3 still leaves
  # the leader's own ack + node-1's ack = 2 = a real 2-of-3 quorum, so the
  # write correctly succeeds (the old fixed-pair phrasing flaked on exactly
  # this). Isolating the actual leader leaves it with only its own ack
  # (1 < min_acks=2) → genuine quorum loss, regardless of who won the
  # election. Distinct failure mode from the EC 4+2 6-node D-5 promotion
  # (which conflates fabric and Raft loss because killing 3 of 6 also
  # breaks Raft majority).
  @integration @multi-node @cross-node @smoke
  Scenario: Write requires 2-of-3 quorum (D-5)
    Given a 3-node kiseki cluster
    And the two non-leader nodes have their incoming fabric denied
    Then a 1MB S3 PUT to the leader fails with quorum lost
    And the leader's fabric_quorum_lost_total ticked at least 1
    And all nodes' incoming fabric is allowed

  # Promoted to @integration. Now reproducible: `write_chunk`
  # early-exits on `min_acks` (the slow peer's PutFragment is
  # still in flight when PUT returns success), so a follower can
  # have the Raft-committed composition delta but not yet the
  # fragment. Read on that follower must local-miss + fabric-fetch.
  # Uses the test-only `POST /admin/test/fabric/slow-ms/{ms}` knob
  # to delay node-3's incoming fabric ack; the PUT returns via
  # min_acks=2 (leader local + node-2) within ~50 ms, then the
  # GET on node-3 races ahead of the in-flight PutFragment and
  # exercises the fabric fallback.
  #
  # Switched from a "slow PutFragment" knob to "deny" — same
  # test intent (node-3 never has the local fragment so GET must
  # fan out), but race-free. Sizing history:
  #   - 1.5 s slow-down: S3 GET retry loop (30 s deadline,
  #     200 ms cadence) routinely outlasted it.
  #   - 60 s slow-down: composition-delta hydration + S3 GET
  #     retry sometimes pushed total elapsed past 60 s on slow
  #     CI runners; the slow PutFragment landed locally and the
  #     assertion saw 0 fabric GET calls.
  #   - 600 s slow-down: still raced against the leader's 10 s
  #     Endpoint timeout — once the leader timed out the slow
  #     call, retried via a different path, OR the slow handler
  #     landed somehow.
  # Deny semantics are deterministic: node-3's PutFragment
  # returns `Status::unavailable` synchronously. With min_acks=2
  # in 3-node Replication-3, leader-self + node-2 still meet
  # quorum, so the PUT returns OK. node-3 NEVER has the local
  # fragment for the rest of the test, so the GET on node-3
  # MUST fan out and the metric MUST tick. The matching
  # `incoming fabric is allowed` step at scenario end clears
  # the deny flag.
  # @flaky: GET retry races against in-flight PutFragment; the GET can
  # win the race and serve from the local fragment cache before the
  # slow PutFragment completes, missing the fabric fan-out the
  # assertion expects. The 60 s slow-down (CLAUDE.md) reduced the
  # flake rate but didn't eliminate it. Cucumber retries up to 2x.
  @integration @multi-node @cross-node @ordering @flaky
  Scenario: Composition delta arrives before fragment (D-10 cross-stream)
    Given a 3-node kiseki cluster
    And node-3's incoming fabric is denied
    When a client writes 1MB via S3 PUT to node-1
    Then S3 GET from node-3 returns the same 1MB
    And node-3 issued at least 1 fabric GetFragment calls for the read
    And node-3's incoming fabric is allowed

  @integration @multi-node @cross-node @leader-change
  Scenario: Refcount preserved across leader change (D-4)
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    And every follower has received the fragment
    And the current leader is killed
    Then a new leader is elected within 15 seconds
    And every chunk of the composition has refcount 1 on the new leader
    And the killed node is restarted and rejoins the cluster

  # Promoted to @integration. The ClusterHarness now ships an mTLS
  # mode (`acquire_cluster_3_mtls`) that generates a CA + per-node
  # fabric certs (SAN URI `spiffe://cluster/fabric/node-{id}`) + a
  # tenant cert (SAN URI `spiffe://cluster/org/...`) via rcgen, and
  # passes the per-node paths to each spawned child via
  # `KISEKI_CA_PATH` / `KISEKI_CERT_PATH` / `KISEKI_KEY_PATH`. The
  # scenario then opens a tonic Channel signed with the TENANT cert
  # and calls `PutFragment` against node-1's data-path port — the
  # SAN-role interceptor (`fabric_san_interceptor`) rejects the call
  # with `PermissionDenied`. Closes I-Auth4 in the BDD harness.
  @integration @multi-node @cross-node @smoke
  Scenario: Tenant cert presented to fabric port is rejected (I-Auth4)
    Given a 3-node mTLS kiseki cluster
    When a tenant cert calls PutFragment against node-1's data-path port
    Then the call is rejected with PermissionDenied

  @integration @degenerate
  Scenario: 1-node cluster degenerates to local-only (D-6)
    Given a running kiseki-server
    When a client writes 1MB via S3 PUT
    Then S3 GET returns the same 1MB
    And no fabric fan-out RPCs were issued
    And the server did not report quorum errors

  # === Shard split/merge admin (ADR-033, ADR-034) ===
  # These scenarios mirror the @library split/merge in storage-admin.feature
  # and log.feature, but drive admin gRPC against a real spawned 3-node
  # cluster — exercising the multiplexed Raft transport (ADR-041) and the
  # cluster-wide consensus path through `RaftShardStore::split_shard` /
  # `merge_shards`. Closes the @library→@integration fidelity gap that
  # the gate-2 audit flagged for shard lifecycle.
  #
  # Contract note (error-taxonomy entry 41b23b5, P3a, GH #223): once
  # watermark-advance GC prunes a shard's delta log (gc boundary > 1),
  # full-replay lifecycle ops — split redistribution, merge copy —
  # REFUSE with the typed `DeltaLogPruned` error (refusal over silent
  # key loss; the compacted-replay unlock is the #223/#220 follow-up).
  # The cluster harness pins KISEKI_WATERMARK_ADVANCE_INTERVAL_MS high
  # on spawned nodes so unrelated suite traffic on this long-lived
  # singleton never advances the bootstrap shard's boundary mid-suite:
  # the three lifecycle scenarios below deterministically pin the
  # UNPRUNED-shard lifecycle, and the refusal contract itself is pinned
  # by the dedicated DeltaLogPruned scenario further down.

  @integration @multi-node @shard-mgmt @smoke
  Scenario: Admin SplitShard returns a new shard id on a 3-node cluster (ADR-033)
    Given a 3-node kiseki cluster
    When the admin calls SplitShard for the bootstrap shard via node-1 admin gRPC
    Then the SplitShard response carries a non-empty right_shard_id distinct from the left
    And the bootstrap shard remains queryable on every node

  @integration @multi-node @shard-mgmt
  Scenario: Admin SplitShard followed by MergeShards round-trips via admin gRPC (ADR-033, ADR-034)
    Given a 3-node kiseki cluster
    When the admin calls SplitShard for the bootstrap shard via node-1 admin gRPC
    And the admin calls MergeShards merging the right back into the left via node-1 admin gRPC
    Then the MergeShards response merged_shard_id equals the left shard id
    And the bootstrap shard remains queryable on every node

  # ADR-033 §4 (Phase B): the control-plane Raft group's apply hook
  # creates the new shard's per-shard Raft group locally on every
  # node — leader explicitly initializes membership after the
  # control-plane RecordSplit commits. Pins that gap closure: pre-#4
  # the new shard existed only on the calling node.
  @integration @multi-node @shard-mgmt @cross-node
  Scenario: SplitShard creates the new shard's per-shard Raft group on every node (ADR-033 §4)
    Given a 3-node kiseki cluster
    When the admin calls SplitShard for the bootstrap shard via node-1 admin gRPC
    Then every node logged the apply hook registering the new shard locally

  # P3a refusal contract (error-taxonomy 41b23b5, GH #223): a shard
  # whose delta log has been pruned (gc boundary > 1) REFUSES
  # full-replay lifecycle ops with the typed `DeltaLogPruned` error —
  # surfaced as gRPC FAILED_PRECONDITION at the admin boundary, and
  # raised fail-fast BEFORE the control-plane RecordSplit so a refused
  # split leaves no dangling topology entry behind. The boundary is
  # advanced via the advance-watermark test knob (the harness pins the
  # supervisor's own advance cadence off — see the contract note
  # above), which drives the REAL replicated AdvanceWatermark + prune
  # path. Runs against a scenario-private 1-shard namespace so the
  # shared singleton's bootstrap shard stays unpruned for the
  # lifecycle scenarios above. Asserts the error CLASS
  # (FAILED_PRECONDITION + the pruned-delta-log marker), not the full
  # message string.
  @integration @multi-node @shard-mgmt
  Scenario: SplitShard on a pruned delta log is refused with the typed DeltaLogPruned error (P3a, GH #223)
    Given a 3-node kiseki cluster
    And a fresh single-shard lifecycle namespace created via admin HTTP
    When the test knob advances the lifecycle shard's hydrator watermark past the replay floor
    Then SplitShard for the lifecycle shard via node-1 admin gRPC is refused as FAILED_PRECONDITION citing a pruned delta log

  # GH #99: `topology namespace-create` fans out the per-shard Raft
  # groups via the control-plane apply hook, but pre-fix NO node called
  # `initialize_membership` for them — every shard sat at
  # `leader_id=null` / `raft_members=[]`, so all writes to the namespace
  # 5xx'd with "leader unavailable". The @library cluster-formation
  # scenarios only assert the shard-MAP placement metadata (the assigned
  # `leader_node` field), never real per-shard raft leadership — which
  # is exactly why this slipped through to the GCP perf run.
  @integration @multi-node @shard-mgmt @cross-node @smoke
  Scenario: Multi-shard namespace-create elects a raft leader for every shard, distributed across nodes (GH #99, #101)
    Given a 3-node kiseki cluster
    When the admin creates a 6-shard namespace via admin HTTP
    Then every shard of that namespace elects a raft leader distributed across the cluster within 20s

  # === Catch-up replication under write volume (GH #255) ===
  #
  # GH #255: catch-up replication wedged PERMANENTLY when an
  # append_entries batch exceeded the hardcoded 128 MiB frame cap.
  # Committer `IncorporateIntents` entries embed full inline payloads
  # (up to DRAIN_BATCH_CAP=1000 intents × 4 KiB per entry); openraft
  # batches replication by entry COUNT only (max_payload_entries=300),
  # so a follower restarting after sustained inline-write volume
  # received catch-up frames of 178–190 MB. The receiver could not
  # drain the oversized body (stream desync) and closed; the leader
  # retried the SAME batch forever — no quorum ever recovered
  # (2026-06-11 GCP run, 1,591 rejections in 3 min, cluster destroyed).
  #
  # The fix byte-budgets replication reads
  # (`FjallRaftLogStore::limited_get_log_entries`, default 32 MiB per
  # batch) and converts the receiver to drain-and-reject (typed
  # per-RPC failure, connection survives). This scenario drives the
  # production shape end to end: high-volume 4 KiB inline writes, one
  # follower stopped mid-volume, more volume while it is down (the
  # leader's log grows ahead), restart — then asserts the follower
  # genuinely catches up (per-shard committed tip convergence) and
  # that NO node ever rejected an oversized Raft RPC.
  @integration @multi-node @cross-node @restart-recovery
  Scenario: Follower restarted under inline-write volume catches up without oversized Raft RPCs (GH #255)
    Given a 6-node kiseki cluster
    And the oversized Raft RPC rejection baseline is recorded
    And a 3-shard namespace "vol255" with leaders distributed across the cluster
    When 12000 distinct 4KB objects are written via S3 across the cluster
    And a follower member of the volume namespace is killed
    And 12000 more distinct 4KB objects are written via S3 across the cluster
    Then the killed node is restarted and rejoins the cluster
    And the restarted node catches up on every volume shard within 120s
    And no node recorded an oversized Raft RPC rejection

  # GH #102 (multi-shard native read AEAD-fail): not reproducible in this
  # harness — the native proxy assumes a uniform per-node data port
  # (`runtime.rs:2203`) but the harness binds random per-node ports, so
  # proxied writes wedge before reads can be exercised. Pinpointing #102
  # needs the real environment (uniform ports + real proxy); see the issue
  # for the GCP+trace plan. A native-proxy peer-list env var would unblock
  # a localhost multi-node repro — tracked separately.
