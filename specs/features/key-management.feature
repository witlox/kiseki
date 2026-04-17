Feature: Key Management — Two-layer encryption, key lifecycle, crypto-shred
  The Key Management context manages all key material across two layers:
  system keys (cluster admin via system key manager) and tenant key
  wrapping (tenant admin via tenant KMS). Handles rotation, epoch
  management, crypto-shred orchestration, and audit of key events.

  Background:
    Given a Kiseki cluster with a system key manager
    And system KEK "sys-kek-001" wrapping system DEKs
    And tenant "org-pharma" with tenant KMS at "kms.pharma.internal"
    And tenant KEK "pharma-kek-001" in epoch 1

  # --- System key lifecycle ---

  Scenario: System DEK generation for chunk encryption
    When a new chunk is written
    Then a system DEK is generated (or retrieved from the current epoch pool)
    And the DEK encrypts the chunk plaintext using AES-256-GCM
    And the DEK is wrapped with the system KEK
    And the wrapped DEK is stored in the chunk envelope
    And the plaintext DEK is held only in memory, never persisted

  Scenario: System KEK rotation
    Given system KEK "sys-kek-001" is in epoch 1
    When the cluster admin triggers system KEK rotation
    Then a new system KEK "sys-kek-002" is generated (epoch 2)
    And new chunks use system DEKs wrapped with "sys-kek-002"
    And existing chunks retain epoch 1 wrapping
    And background re-wrapping migrates epoch 1 DEK wrappings to epoch 2
    And both epochs are valid until migration completes
    And the rotation event is recorded in the audit log

  # --- Tenant key wrapping ---

  Scenario: Tenant KEK wraps system DEK for tenant access
    Given chunk "abc123" is encrypted with system DEK "dek-42"
    And "dek-42" is wrapped with system KEK "sys-kek-001"
    When "org-pharma" needs access to "abc123"
    Then "dek-42" is also wrapped with tenant KEK "pharma-kek-001"
    And "org-pharma" can: unwrap "dek-42" with their KEK → decrypt chunk
    And the system can: unwrap "dek-42" with system KEK → but only for system operations
    And both wrappings coexist in the envelope

  Scenario: Tenant without KEK cannot access any chunks
    Given a new tenant "org-newco" has been created but has not configured a KMS
    When "org-newco" attempts to read a chunk
    Then the read fails with "tenant KMS not configured" error
    And no data is returned
    And the access attempt is recorded in the audit log

  # --- Tenant key rotation (epoch-based) ---

  Scenario: Epoch-based tenant key rotation
    Given "org-pharma" tenant KEK "pharma-kek-001" is epoch 1
    When the tenant admin rotates the tenant KEK
    Then a new KEK "pharma-kek-002" is generated (epoch 2) in the tenant KMS
    And new chunks get system DEK wrappings under epoch 2 tenant KEK
    And existing chunks retain epoch 1 tenant KEK wrapping
    And background re-wrapping migrates epoch 1 wrappings to epoch 2
    And both epochs are valid during migration
    And old data remains accessible throughout rotation
    And the rotation event is recorded in the audit log (tenant export)

  Scenario: Full re-encryption triggered by admin (key compromise)
    Given "org-pharma" suspects key compromise of "pharma-kek-001"
    When the tenant admin triggers full re-encryption
    Then all chunks referenced by "org-pharma" are:
      | step | action                                              |
      | 1    | decrypted using system DEK (unwrapped via old KEK)  |
      | 2    | re-encrypted with a new system DEK                  |
      | 3    | new DEK wrapped with new tenant KEK (epoch 2)       |
    And old system DEKs for affected chunks are destroyed
    And old tenant KEK wrappings are destroyed
    And the operation runs in background with progress tracking
    And the re-encryption event is recorded in the audit log

  # --- Crypto-shred ---

  Scenario: Crypto-shred destroys tenant KEK
    Given "org-pharma" has chunks [c1, c2, c3] with refcounts [2, 1, 1]
    When the tenant admin performs crypto-shred for "org-pharma"
    Then tenant KEK "pharma-kek-001" is destroyed in the tenant KMS
    And all tenant KEK wrappings for "org-pharma" become invalid
    And system DEKs can no longer be unwrapped via tenant path
    And chunks remain on storage as system-encrypted ciphertext
    And refcounts for "org-pharma"'s references are decremented
    And the crypto-shred event is recorded in the audit log (system + tenant export)

  Scenario: Crypto-shred with retention hold preserves ciphertext
    Given a retention hold "hipaa-7yr" is active on "org-pharma" namespace "trials"
    When crypto-shred is performed for "org-pharma"
    Then tenant KEK is destroyed (data unreadable)
    And chunks with refcount 0 are NOT physically deleted (hold active)
    And system-encrypted ciphertext is retained until hold expires
    And the hold-preserving-after-shred state is recorded in the audit log

  Scenario: Crypto-shred does not affect other tenants' access
    Given chunk "shared-99" has refcount 2 (org-pharma and org-biotech, cross-tenant dedup)
    When "org-pharma" performs crypto-shred
    Then "org-pharma"'s KEK wrapping for "shared-99" is invalidated
    And "org-biotech"'s KEK wrapping remains valid
    And "org-biotech" can still read "shared-99"
    And "shared-99" refcount decrements to 1
    And "shared-99" is NOT eligible for GC (refcount > 0)

  # --- KMS connectivity ---

  Scenario: Tenant KMS temporarily unreachable — cached keys sustain operations
    Given "org-pharma" KMS is unreachable
    And cached tenant KEK material has a TTL of 300 seconds
    When a read request arrives for "org-pharma" data within the cache window
    Then the cached KEK is used to unwrap the system DEK
    And the read succeeds
    And a warning is logged: "tenant KMS unreachable, using cached key material"

  Scenario: Tenant KMS unreachable — cache expired
    Given "org-pharma" KMS has been unreachable for 600 seconds
    And the cached KEK TTL of 300 seconds has expired
    When a read request arrives for "org-pharma" data
    Then the read fails with "tenant KMS unavailable, key cache expired" error
    And the tenant admin and cluster admin are alerted
    And no stale key material is used beyond the TTL

  Scenario: Tenant KMS reachable from federated site
    Given "org-pharma" has data at site-EU and site-CH
    And tenant KMS is at "kms.pharma.internal"
    When site-CH needs to decrypt "org-pharma" data
    Then site-CH contacts "kms.pharma.internal" over encrypted channel
    And obtains tenant KEK wrapping for the requested system DEK
    And decryption proceeds using the unwrapped DEK
    And the KMS connection is authenticated and encrypted end-to-end

  # --- Key audit ---

  Scenario: All key lifecycle events are audited
    Given any key event occurs:
      | event_type       | example                        |
      | key_generation   | new system DEK created         |
      | key_rotation     | tenant KEK rotated             |
      | key_destruction  | crypto-shred                   |
      | key_access       | system DEK unwrapped for read  |
      | re_encryption    | full re-encryption triggered    |
    Then the event is recorded in the audit log with:
      | field      | value                          |
      | timestamp  | ISO 8601 with timezone         |
      | actor      | tenant admin / cluster admin / system |
      | key_id     | affected key identifier        |
      | event_type | from table above               |
      | tenant_id  | if tenant-scoped               |
    And the event is included in the tenant audit export (if tenant-scoped)
    And keys themselves are NEVER recorded in the audit log

  # --- Failure modes ---

  Scenario: Tenant KMS permanently lost — unrecoverable
    Given "org-pharma" KMS infrastructure is destroyed
    And "org-pharma" has no KMS backups
    When any operation requiring "org-pharma" tenant KEK is attempted
    Then the operation fails permanently
    And all "org-pharma" data is unreadable (system-encrypted but tenant-unwrappable)
    And the cluster admin is alerted
    And Kiseki does not provide key escrow or recovery
    And the loss is documented as tenant responsibility per I-K11

  Scenario: System key manager failure
    Given the system key manager is an internal HA Kiseki service
    And the system key manager loses quorum
    When a new chunk write requires a system DEK
    Then the write fails with retriable error
    And cached system DEKs for reads may still work within cache TTL
    And the cluster admin is alerted immediately (highest severity)
    And no data is written without proper system encryption
    And this is a cluster-wide write outage until quorum is restored

  # --- Edge cases ---

  Scenario: Concurrent key rotation and crypto-shred
    Given "org-pharma" tenant admin initiates key rotation
    And simultaneously another admin initiates crypto-shred
    Then exactly one operation succeeds (serialized via tenant KMS)
    And if rotation wins: rotation completes, then shred can proceed with new KEK
    And if shred wins: KEK is destroyed, rotation is moot
    And the outcome is deterministic and audited

  Scenario: Key epoch mismatch during read
    Given chunk "c50" was written in epoch 1
    And the current epoch is 3
    And epoch 1 KEK wrapping has not yet been migrated
    When a read for "c50" is requested
    Then the system retrieves the epoch 1 tenant KEK wrapping
    And unwraps the system DEK using epoch 1 material
    And the read succeeds
    And the chunk is flagged for background re-wrapping to epoch 3
