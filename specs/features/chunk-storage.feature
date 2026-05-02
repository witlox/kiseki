Feature: Chunk Storage - Encrypted chunk persistence, placement, and lifecycle
  The Chunk Storage context stores and retrieves opaque encrypted chunks,
  manages placement across affinity pools, handles replication/EC, runs
  GC based on refcounts, and enforces retention holds.

  Background:
    Given a Kiseki cluster with 3 affinity pools:
      | pool       | device_class | durability | devices |
      | fast-nvme  | NVMe-U.2     | EC 4+2     | 24      |
      | bulk-nvme  | NVMe-QLC     | EC 8+3     | 48      |
      | meta-nvme  | NVMe-U.2     | replicate-3 | 12      |
    And tenant "org-pharma" exists with cross-tenant dedup enabled (default)
    And tenant "org-defense" exists with cross-tenant dedup opted out (HMAC chunk IDs)

  # --- Happy path: chunk write ---

  # @unit Scenario "Write a chunk with HMAC ID" moved to crate-level unit test:
  # kiseki-crypto/src/chunk_id.rs::hmac_chunk_id_unique_per_tenant_no_cross_dedup

  # --- Placement and affinity ---

  @library
  Scenario: Pool capacity exhausted triggers rebalance
    Given pool "fast-nvme" is at 95% capacity
    When a new chunk targets "fast-nvme"
    Then the chunk is placed in "fast-nvme" if space exists after cleanup
    And the control plane is notified to trigger data migration to "bulk-nvme" if needed
    And the chunk write is not silently redirected without policy approval

  # --- GC and refcounting ---

  # --- Repair and failure ---

  @library
  Scenario: Device failure triggers chunk repair
    Given device "nvme-17" in pool "fast-nvme" fails
    And chunks [c10, c11, c12] had EC fragments on "nvme-17"
    When a DeviceFailure event is detected
    Then repair is triggered for affected chunks
    And EC parity is used to reconstruct the missing fragments
    And repaired fragments are placed on healthy devices in the pool
    And chunk availability is restored

  @library
  Scenario: Chunk unrecoverable - insufficient EC parity
    Given chunk "c99" has EC 4+2 encoding
    And 3 of 6 fragments are lost (exceeds parity tolerance of 2)
    When repair is attempted
    Then repair fails
    And a ChunkLost event is emitted
    And the Composition context is notified that compositions referencing "c99" have data loss
    And the cluster admin is alerted

  @library
  Scenario: Admin-triggered chunk repair
    Given the cluster admin suspects corruption on device "nvme-22"
    When the admin triggers RepairChunk for all chunks on "nvme-22"
    Then each chunk's EC/replication integrity is verified
    And any corrupted fragments are rebuilt from parity
    And the operation is recorded in the audit log

  # --- Encryption invariant enforcement ---

  # --- Edge cases ---

  @library
  Scenario: Chunk write during pool rebalance
    Given pool "fast-nvme" is rebalancing (migrating chunks to "bulk-nvme")
    When a new chunk targets "fast-nvme"
    Then the chunk is written to "fast-nvme" if capacity allows
    And the rebalance continues independently
    And the new chunk is not automatically included in the migration

  # --- Workflow Advisory integration (ADR-020) ---
  # Chunk Storage acts on affinity / prefetch / dedup-intent / retention-intent
  # hints and emits locality-class and pool-backpressure telemetry to the
  # caller. Hints are preferences; placement remains server-authoritative
  # (I-WA9). Ownership is checked before any telemetry is computed (I-WA6).

  # @unit scenarios moved to crate-level unit tests:
  # "Affinity hint preference" → kiseki-chunk/src/store.rs::placement_works_without_affinity_hints
  # "Dedup-intent { per-rank }" → kiseki-chunk/src/store.rs::dedup_intent_per_rank_skips_dedup
  # "Dedup-intent { shared-ensemble }" → kiseki-chunk/src/store.rs::dedup_intent_shared_ensemble_uses_normal_dedup
  # "Locality-class telemetry" → kiseki-chunk/src/store.rs::locality_class_telemetry_shape
  # "Pool backpressure k-anonymity" → kiseki-chunk/src/store.rs::pool_backpressure_k_anonymity_sentinel

  @library
  Scenario: Repair-degraded read emits telemetry without leaking topology
    Given a chunk in the caller's composition is being read while EC repair is in progress
    When the read succeeds from the remaining shards
    Then a repair-degraded warning telemetry event is emitted to the caller's workflow
    And the event contains only { composition_id, degraded: true, severity: advisory } - no device, node, or parity-shard identifiers (I-WA11)

  # --- Cross-node placement and fabric fallback (Phase 16a) ---
  #
  # Replication-N is the only durability strategy in 16a; EC fragment
  # distribution lands in 16b. Each peer holds the whole envelope at
  # `fragment_index = 0`. Spec: phase-16-cross-node-chunks.md (rev 4),
  # ADR-005, ADR-026.

  @integration @multi-node @cross-node
  Scenario: Replication-3 places one fragment on each node
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    And every follower has received the fragment
    Then every chunk of the composition has a fragment on node-1
    And every chunk of the composition has a fragment on node-2
    And every chunk of the composition has a fragment on node-3
    And the cluster placement for every chunk lists all 3 nodes

  # DEFERRED — needs deterministic missing-fragment induction (delete
  # a single fragment from one node's local store mid-test). Driving
  # this naturally via cross-stream lag is flaky. Promote when
  # `kiseki-control inspect-chunk` (task in progress) gains a
  # `--drop-local` debug action OR the local-store gains a fault-injection
  # knob that operators use for chaos drills.
  @library @cross-node
  Scenario: Read falls back to fabric when local fragment is missing
    Given a chunk replicated to [node-1, node-2, node-3]
    And node-2's local store is missing the fragment (cross-stream lag)
    When a read is issued against node-2
    Then node-2 first tries its local store — miss
    Then node-2 calls `GetFragment` against node-1
    And on success returns the envelope to the caller
    And `kiseki_fabric_ops_total{op="get",peer="node-1",outcome="ok"}` increments

  @integration @multi-node @cross-node
  Scenario: GC marks chunks tombstoned on every node when refcount drops to 0 (I-C2)
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    And every follower has received the fragment
    And the composition is deleted via S3 DELETE on node-1
    Then within 30 seconds every chunk's refcount on the leader drops to 0
    And every chunk is tombstoned in the cluster state on every node
    # Local fragment deletion is on the orphan-fragment scrub
    # cadence (10 min per shard) — too slow for BDD. The tombstone
    # invariant is what guarantees the scrub will eventually clean
    # up; the periodic e2e perf cluster catches any stuck-fragment
    # regressions in real time.

  # --- Multi-node integration (requires multi-server harness) ---

  @integration
  Scenario: S3 PUT on single-node server stores data locally
    Given a running kiseki-server
    When a client writes 1MB via S3 PUT
    Then S3 GET returns the same 1MB
    And the server did not report quorum errors

  @integration @multi-node
  Scenario: S3 PUT on 3-node cluster replicates to all nodes
    Given a 3-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    Then S3 GET from node-2 returns the same 1MB
    And S3 GET from node-3 returns the same 1MB

  @integration @multi-node
  Scenario: Writes resume on new leader after leader kill
    Given a 3-node kiseki cluster
    When the current leader is killed
    Then a new leader is elected within 15 seconds
    When a client writes 1MB via S3 PUT to the cluster
    Then S3 GET from any surviving node returns the same 1MB
    And the killed node is restarted and rejoins the cluster

  # Regression witness for the GCP 2026-05-02 perf-cluster failure: a
  # 6-node cluster takes the EC 4+2 code path (defaults_for(>=6)),
  # which 3-node tests don't exercise. The GCP run surfaced ~83%
  # PutFragment unavailable + 1760 quorum-lost events + zero log lines.
  # This scenario must succeed on the same configuration before we
  # claim cross-node fabric is production-ready at scale.
  @integration @multi-node @ec
  Scenario: 6-node cluster — PUT lands EC 4+2 fragments without quorum loss
    Given a 6-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    Then the leader's fabric_quorum_lost_total stays at zero
    And S3 GET from node-2 returns the same 1MB

  # Scale-out witness. The 6-node case is degenerate for EC: every
  # chunk's placement is the full {1..6} set, so the rendezvous-hash
  # placement selector is never exercised. 20 nodes pick 6 of 20 per
  # chunk — the placement-routing path that hides bugs the 6-node
  # test can't surface.
  @integration @multi-node @ec @slow
  Scenario: 20-node cluster — PUT routes via rendezvous-hash placement
    Given a 20-node kiseki cluster
    When a client writes 1MB via S3 PUT to node-1
    Then the leader's fabric_quorum_lost_total stays at zero
    And S3 GET from node-2 returns the same 1MB
