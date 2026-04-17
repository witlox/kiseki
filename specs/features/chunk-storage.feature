Feature: Chunk Storage — Encrypted chunk persistence, placement, and lifecycle
  The Chunk Storage context stores and retrieves opaque encrypted chunks,
  manages placement across affinity pools, handles replication/EC, runs
  GC based on refcounts, and enforces retention holds.

  Background:
    Given a Kiseki cluster with 3 affinity pools:
      | pool       | device_class | durability | devices |
      | fast-nvme  | NVMe-U.2     | EC 4+2     | 24      |
      | bulk-nvme  | NVMe-QLC     | EC 8+3     | 48      |
      | meta-nvme  | NVMe-U.2     | replicate-3| 12      |
    And tenant "org-pharma" exists with cross-tenant dedup enabled (default)
    And tenant "org-defense" exists with cross-tenant dedup opted out (HMAC chunk IDs)

  # --- Happy path: chunk write ---

  Scenario: Write a chunk with content-addressed ID (default tenant)
    Given the Composition context for "org-pharma" submits plaintext data
    When the system computes chunk_id = sha256(plaintext)
    And encrypts the plaintext with a system DEK
    And stores the ciphertext in pool "fast-nvme" per affinity policy
    Then a ChunkStored event is emitted with the chunk_id
    And the chunk's refcount is initialized to 1
    And the envelope contains: ciphertext, system DEK reference, algorithm_id, key_epoch
    And no plaintext is persisted at any point

  Scenario: Write a chunk with HMAC ID (opted-out tenant)
    Given the Composition context for "org-defense" submits plaintext data
    When the system computes chunk_id = HMAC(plaintext, org-defense_tenant_key)
    And encrypts the plaintext with a system DEK
    And stores the ciphertext in pool "fast-nvme"
    Then the chunk_id is unique to "org-defense"
    And the same plaintext from another tenant would produce a different chunk_id
    And cross-tenant dedup cannot match this chunk

  Scenario: Dedup — existing chunk referenced by new composition
    Given "org-pharma" has a chunk with chunk_id "abc123" and refcount 1
    When a new composition in "org-pharma" references the same plaintext
    And chunk_id = sha256(plaintext) = "abc123"
    Then no new chunk is written
    And the existing chunk's refcount is incremented to 2
    And the new composition receives a reference to "abc123"

  Scenario: Cross-tenant dedup for default tenants
    Given "org-pharma" has chunk "abc123" with refcount 1
    And another default tenant "org-biotech" writes the same plaintext
    And chunk_id = sha256(plaintext) = "abc123"
    Then no new chunk is written
    And chunk "abc123" refcount is incremented to 2
    And "org-biotech" receives a tenant KEK wrapping of the system DEK for "abc123"
    And "org-pharma" and "org-biotech" each have independent key-wrapping paths

  # --- Chunk read ---

  Scenario: Read an encrypted chunk
    Given chunk "abc123" exists in pool "fast-nvme"
    When a stream processor requests ReadChunk for "abc123"
    Then the encrypted chunk envelope is returned
    And the caller unwraps using: tenant KEK → system DEK → decrypt ciphertext
    And no plaintext is transmitted on the wire

  # --- Placement and affinity ---

  Scenario: Chunk placed according to affinity policy
    Given a composition's view descriptor specifies tier "fast-nvme" for data
    When a chunk is written for that composition
    Then the chunk is placed in pool "fast-nvme"
    And EC 4+2 encoding is applied per pool policy
    And the chunk's fragments are distributed across devices in the pool

  Scenario: Pool capacity exhausted triggers rebalance
    Given pool "fast-nvme" is at 95% capacity
    When a new chunk targets "fast-nvme"
    Then the chunk is placed in "fast-nvme" if space exists after cleanup
    Or the control plane is notified to trigger data migration to "bulk-nvme"
    And the chunk write is not silently redirected without policy approval

  # --- GC and refcounting ---

  Scenario: Chunk GC when refcount reaches zero
    Given chunk "abc123" has refcount 1
    And no retention hold is active on "abc123"
    When the last composition referencing "abc123" is deleted
    Then refcount drops to 0
    And "abc123" becomes eligible for physical GC
    And the GC process eventually deletes the ciphertext from storage

  Scenario: Chunk GC blocked by retention hold
    Given chunk "abc123" has refcount 0
    And a retention hold "hipaa-litigation-2026" is active on "abc123"
    When the GC process evaluates "abc123"
    Then "abc123" is NOT deleted
    And it remains on storage as system-encrypted ciphertext
    And GC re-evaluates after the hold expires or is released

  Scenario: Retention hold set before crypto-shred
    Given tenant "org-pharma" has compositions referencing chunks [c1, c2, c3]
    And a retention hold "gdpr-retention-7yr" is set on namespace "patient-data"
    When "org-pharma" performs crypto-shred (destroys tenant KEK)
    Then chunks [c1, c2, c3] are unreadable (no tenant key to unwrap system DEK)
    And refcounts decrement as composition references are invalidated
    And chunks with refcount 0 are NOT GC'd due to retention hold
    And chunks remain as system-encrypted ciphertext until hold expires

  Scenario: Crypto-shred without retention hold — chunks GC'd
    Given tenant "org-temp" has compositions referencing chunks [c4, c5]
    And no retention hold is active
    When "org-temp" performs crypto-shred
    Then chunks are unreadable immediately
    And refcounts drop to 0
    And chunks become eligible for physical GC
    And GC eventually reclaims storage

  # --- Repair and failure ---

  Scenario: Device failure triggers chunk repair
    Given device "nvme-17" in pool "fast-nvme" fails
    And chunks [c10, c11, c12] had EC fragments on "nvme-17"
    When a DeviceFailure event is detected
    Then repair is triggered for affected chunks
    And EC parity is used to reconstruct the missing fragments
    And repaired fragments are placed on healthy devices in the pool
    And chunk availability is restored

  Scenario: Chunk unrecoverable — insufficient EC parity
    Given chunk "c99" has EC 4+2 encoding
    And 3 of 6 fragments are lost (exceeds parity tolerance of 2)
    When repair is attempted
    Then repair fails
    And a ChunkLost event is emitted
    And the Composition context is notified that compositions referencing "c99" have data loss
    And the cluster admin is alerted

  Scenario: Admin-triggered chunk repair
    Given the cluster admin suspects corruption on device "nvme-22"
    When the admin triggers RepairChunk for all chunks on "nvme-22"
    Then each chunk's EC/replication integrity is verified
    And any corrupted fragments are rebuilt from parity
    And the operation is recorded in the audit log

  # --- Encryption invariant enforcement ---

  Scenario: Plaintext never reaches storage
    Given a chunk write is in progress
    When the system DEK encryption step fails (e.g., HSM timeout)
    Then the chunk write is aborted
    And no data — plaintext or partial ciphertext — is persisted
    And the Composition context receives a retriable error

  Scenario: Chunk envelope integrity verification on read
    Given chunk "abc123" is read from storage
    When the authenticated encryption tag is verified
    Then if verification succeeds, the chunk is returned
    And if verification fails, the chunk is flagged as corrupted
    And a repair is triggered from EC parity or replicas
    And the corruption event is recorded in the audit log

  # --- Edge cases ---

  Scenario: Concurrent dedup — two writers for same chunk_id simultaneously
    Given two compositions in "org-pharma" write the same plaintext concurrently
    And both compute chunk_id = "abc123"
    Then chunk writes are idempotent:
      | writer | chunk exists? | action                    | result     |
      | first  | no            | store ciphertext, refcount=1 | success |
      | second | yes           | increment refcount to 2   | success    |
    And no rejection or retry is needed
    And no duplicate ciphertext is stored

  Scenario: Chunk write during pool rebalance
    Given pool "fast-nvme" is rebalancing (migrating chunks to "bulk-nvme")
    When a new chunk targets "fast-nvme"
    Then the chunk is written to "fast-nvme" if capacity allows
    And the rebalance continues independently
    And the new chunk is not automatically included in the migration
