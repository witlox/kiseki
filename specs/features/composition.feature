Feature: Composition — Tenant-scoped data assembly and namespace management
  The Composition context maintains metadata structures describing how
  chunks assemble into data units (files, objects). It mediates all
  writes: translating protocol-level operations into deltas for the Log
  and chunk writes for Chunk Storage. Manages namespaces and refcounting.

  Background:
    Given a Kiseki cluster with tenant "org-pharma"
    And namespace "trials" in shard "shard-trials-1"
    And tenant KEK "pharma-kek-001" is active

  # --- Happy path: create composition ---

  Scenario: Create a new file composition via protocol gateway
    Given the protocol gateway receives an NFS CREATE for "/trials/study-42/results.h5"
    When the Composition context processes the create:
      | step | action                                                |
      | 1    | compute chunk_id = sha256(file_data)                  |
      | 2    | write chunk to Chunk Storage (idempotent)             |
      | 3    | receive ChunkStored confirmation                      |
      | 4    | append delta to shard "shard-trials-1" with:          |
      |      |   header: hashed_key, create op, chunk_id reference   |
      |      |   payload: encrypted filename, attributes             |
      | 5    | receive DeltaCommitted with sequence_number            |
    Then the composition "results.h5" exists in namespace "trials"
    And the chunk's refcount includes this composition's reference
    And the protocol gateway receives success

  Scenario: Create a small file with inline data
    Given the protocol gateway receives a CREATE for a 512-byte file
    And the inline data threshold is 4096 bytes
    When the Composition context processes the create
    Then no chunk is written to Chunk Storage
    And the file data is included inline in the delta's encrypted payload
    And the delta is committed to the shard
    And the composition is complete with inline data only

  # --- Write path: update composition ---

  Scenario: Append data to an existing composition
    Given composition "results.h5" exists with chunks [c1, c2]
    When a 64MB append is written
    Then new chunks [c3, c4] are written to Chunk Storage
    And a delta is appended: "composition results.h5 extended with [c3, c4]"
    And the composition now references [c1, c2, c3, c4]
    And refcounts for c3, c4 are initialized to 1

  Scenario: Overwrite a byte range in a composition
    Given composition "model.bin" exists with chunks [c1, c2, c3]
    And chunk c2 covers byte range 64MB-128MB
    When a write modifies bytes 80MB-90MB
    Then a new chunk c2' is written covering the modified range
    And a delta records: "composition model.bin: c2 replaced by c2' for range 80M-90M"
    And c2 refcount is decremented (if no other composition references it)
    And c2' refcount is initialized to 1

  # --- Multipart / bulk write ---

  Scenario: S3 multipart upload
    Given the protocol gateway receives an S3 CreateMultipartUpload
    When parts are uploaded in parallel:
      | part | chunk_id | status   |
      | 1    | c10      | stored   |
      | 2    | c11      | stored   |
      | 3    | c12      | stored   |
    And the protocol gateway sends CompleteMultipartUpload
    Then the Composition context verifies all chunks are durable
    And a single delta records the complete composition: [c10, c11, c12]
    And the composition becomes visible to readers only after the finalize delta commits
    And individual parts are NOT visible before completion (I-L5)

  Scenario: Multipart upload aborted
    Given a multipart upload is in progress with chunks [c10, c11] stored
    When the protocol gateway sends AbortMultipartUpload
    Then no finalize delta is committed
    And chunks c10, c11 have refcount 0 (no composition references them)
    And chunks become eligible for GC

  # --- Delete ---

  Scenario: Delete a composition
    Given composition "old-results.csv" references chunks [c5, c6]
    And c5 has refcount 2 (shared with another composition)
    And c6 has refcount 1
    When the Composition context processes a DELETE
    Then a tombstone delta is appended to the shard
    And c5 refcount is decremented to 1 (still referenced elsewhere)
    And c6 refcount is decremented to 0 (eligible for GC if no hold)
    And the composition is no longer visible in the namespace

  Scenario: Delete composition with object versioning enabled
    Given namespace "trials" has object versioning enabled
    And composition "results.h5" has versions [v1, v2, v3]
    When a DELETE is issued for "results.h5"
    Then a delete marker is appended (tombstone delta)
    And the current version becomes the delete marker
    And previous versions [v1, v2, v3] remain accessible by version ID
    And chunk refcounts are NOT decremented (versions still reference them)

  # --- Dedup ---

  Scenario: Intra-tenant dedup — same data written twice
    Given "org-pharma" writes file A with plaintext P (chunk_id = sha256(P) = "abc")
    And later writes file B with the same plaintext P
    Then file B's composition references chunk "abc"
    And chunk "abc" refcount is 2
    And no new chunk is stored

  Scenario: Cross-tenant dedup (default tenants)
    Given "org-pharma" has chunk "abc" (refcount 1)
    And "org-biotech" (default dedup) writes the same plaintext
    Then chunk "abc" refcount increments to 2
    And "org-biotech" receives a tenant KEK wrapping for the system DEK
    And one copy of ciphertext serves both tenants

  Scenario: No cross-tenant dedup for opted-out tenant
    Given "org-defense" (HMAC chunk IDs) writes plaintext P
    And chunk_id = HMAC(P, org-defense_key) = "def456"
    And "org-pharma" has chunk sha256(P) = "abc123"
    Then "def456" != "abc123" — no dedup match
    And a new chunk "def456" is stored for "org-defense"
    And "org-defense" data is fully isolated

  # --- Namespace management ---

  Scenario: Create namespace
    Given tenant admin for "org-pharma" requests new namespace "genomics"
    When the Control Plane approves (quota, policy check)
    Then a new shard is created for "genomics"
    And the namespace is associated with the tenant and shard
    And compliance tags from the org level are inherited

  Scenario: Namespace inherits compliance tags
    Given org "org-pharma" has compliance tags [HIPAA, GDPR]
    And namespace "trials" has additional tag [revFADP]
    Then the effective compliance regime for "trials" is [HIPAA, GDPR, revFADP]
    And the staleness floor is the strictest of the three regimes
    And audit requirements are the union of all three

  # --- Failure paths ---

  Scenario: Chunk write fails during composition create
    Given the Composition context is creating a new composition
    And chunk write to Chunk Storage fails (pool full, system key manager down)
    Then the composition create is aborted
    And no delta is committed to the Log
    And the protocol gateway receives a retriable error
    And no partial state remains

  Scenario: Delta commit fails after chunk write succeeds
    Given chunk c20 was successfully written (refcount 1)
    And the subsequent delta commit to the Log fails (shard unavailable)
    Then the composition create fails
    And the protocol gateway receives a retriable error
    And chunk c20 has refcount 0 (no composition references it)
    And c20 becomes eligible for GC (orphan chunk cleanup)

  Scenario: Cross-shard rename returns EXDEV
    Given composition "file.txt" exists in namespace "alpha" (shard-1)
    When a POSIX rename targets namespace "beta" (shard-2)
    Then the operation returns EXDEV
    And the caller handles via copy + delete
    And no 2PC or cross-shard coordination occurs
