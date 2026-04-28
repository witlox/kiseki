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

  @integration
  Scenario: Pool capacity exhausted triggers rebalance
    Given pool "fast-nvme" is at 95% capacity
    When a new chunk targets "fast-nvme"
    Then the chunk is placed in "fast-nvme" if space exists after cleanup
    And the control plane is notified to trigger data migration to "bulk-nvme" if needed
    And the chunk write is not silently redirected without policy approval

  # --- GC and refcounting ---

  # --- Repair and failure ---

  @integration
  Scenario: Device failure triggers chunk repair
    Given device "nvme-17" in pool "fast-nvme" fails
    And chunks [c10, c11, c12] had EC fragments on "nvme-17"
    When a DeviceFailure event is detected
    Then repair is triggered for affected chunks
    And EC parity is used to reconstruct the missing fragments
    And repaired fragments are placed on healthy devices in the pool
    And chunk availability is restored

  @integration
  Scenario: Chunk unrecoverable - insufficient EC parity
    Given chunk "c99" has EC 4+2 encoding
    And 3 of 6 fragments are lost (exceeds parity tolerance of 2)
    When repair is attempted
    Then repair fails
    And a ChunkLost event is emitted
    And the Composition context is notified that compositions referencing "c99" have data loss
    And the cluster admin is alerted

  @integration
  Scenario: Admin-triggered chunk repair
    Given the cluster admin suspects corruption on device "nvme-22"
    When the admin triggers RepairChunk for all chunks on "nvme-22"
    Then each chunk's EC/replication integrity is verified
    And any corrupted fragments are rebuilt from parity
    And the operation is recorded in the audit log

  # --- Encryption invariant enforcement ---

  # --- Edge cases ---

  @integration
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

  @integration
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

  @integration @cross-node
  Scenario: Replication-N places one fragment per peer
    Given a 3-node cluster and pool "default" with `Replication { copies: 3 }`
    When a client writes a chunk to "default"
    Then exactly 3 fragments exist — one on each of [node-1, node-2, node-3]
    And every fragment is the same encrypted envelope (content-addressed)
    And `cluster_chunk_state[("default", chunk_id)].placement` lists all 3 nodes

  @integration @cross-node
  Scenario: Read falls back to fabric when local fragment is missing
    Given a chunk replicated to [node-1, node-2, node-3]
    And node-2's local store is missing the fragment (cross-stream lag)
    When a read is issued against node-2
    Then node-2 first tries its local store — miss
    Then node-2 calls `GetFragment` against node-1
    And on success returns the envelope to the caller
    And `kiseki_fabric_ops_total{op="get",peer="node-1",outcome="ok"}` increments

  @integration @cross-node
  Scenario: GC across peers when refcount reaches 0 (I-C2)
    Given chunk "c-gc" has refcount=1 on every node
    When the only composition referencing "c-gc" is deleted
    Then the cluster_chunk_state refcount transitions to 0
    And the leader sends `DeleteFragment` to every peer in the placement list
    And after local GC sweep the chunk is removed from every node's local store
