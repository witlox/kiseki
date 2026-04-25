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

  @unit
  Scenario: Write a chunk with HMAC ID (opted-out tenant)
    Given the Composition context for "org-defense" submits plaintext data
    When the system computes chunk_id = HMAC(plaintext, org-defense_tenant_key)
    And encrypts the plaintext with a system DEK
    And stores the ciphertext in pool "fast-nvme"
    Then the chunk_id is unique to "org-defense"
    And the same plaintext from another tenant would produce a different chunk_id
    And cross-tenant dedup cannot match this chunk

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

  @unit
  Scenario: Affinity hint preference honoured within policy
    Given workload "training-run-42" is authorised for pools [fast-nvme, bulk-nvme]
    And a new chunk is being placed for composition "checkpoint.pt"
    And the caller has attached hint { affinity_pool: "fast-nvme", colocate_rack: "rack-7" }
    When the placement engine runs
    Then the chunk MAY be placed in fast-nvme on rack-7 when durability and retention constraints allow
    And the engine MAY override the hint to satisfy I-C3 (policy), I-C4 (durability), or I-C2b (retention hold)
    And hints never cause placement in a pool the workload is not authorised for (I-WA14)

  @unit
  Scenario: Dedup-intent hint { per-rank } skips dedup path for scratch chunks
    Given workload "training-run-42" writes per-rank scratch output
    And the caller attaches hint { dedup_intent: per-rank }
    When the chunk is presented for storage
    Then the dedup refcount path is bypassed (no sha256 lookup; new chunk allocated)
    And the chunk ID is still derived per I-K10 (tenant HMAC if opted out, sha256 otherwise)
    And subsequent writes of identical plaintext by the same workload do NOT coalesce via dedup (hint honoured)
    And tenant dedup policy (I-X2) is never violated regardless of hint

  @unit
  Scenario: Dedup-intent hint { shared-ensemble } uses normal dedup path
    Given workload "training-run-42" writes ensemble-broadcast input data
    And the caller attaches hint { dedup_intent: shared-ensemble }
    When the chunk is presented for storage
    Then the dedup refcount path is used normally, bounded by I-X2 tenant policy
    And the hint never enables cross-tenant dedup when tenant policy opts out (I-K10)

  @unit
  Scenario: Locality-class telemetry for caller-owned chunks
    Given workload "training-run-42" reads a 1GB composition spanning 64 chunks on mixed placement
    When the caller requests LocalityTelemetry for the composition
    Then the response classifies each chunk into one of {local-node, local-rack, same-pool, remote, degraded}
    And no node ID, rack label, device serial, or pool utilisation metric is returned (I-WA11)
    And only chunks owned by the caller's workload are included; unauthorised targets return the same shape as absent chunks (I-WA6)

  @unit
  Scenario: Pool backpressure telemetry uses k-anonymity bucketing under low-k
    Given pool "fast-nvme" hosts chunks from workload "training-run-42" and three neighbour workloads (k=4 < 5)
    When the caller subscribes to pool-backpressure telemetry for "fast-nvme"
    Then the response shape is identical to the populated-k case
    And neighbour-derived fields carry the fixed sentinel value defined by policy (I-WA5)
    And no timing or size variation reveals the actual k

  @integration
  Scenario: Repair-degraded read emits telemetry without leaking topology
    Given a chunk in the caller's composition is being read while EC repair is in progress
    When the read succeeds from the remaining shards
    Then a repair-degraded warning telemetry event is emitted to the caller's workflow
    And the event contains only { composition_id, degraded: true, severity: advisory } - no device, node, or parity-shard identifiers (I-WA11)
