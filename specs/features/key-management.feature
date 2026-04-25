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

  # --- Crypto-shred ---

  @integration
  Scenario: Crypto-shred destroys tenant KEK
    Given "org-pharma" has chunks [c1, c2, c3] with refcounts [2, 1, 1]
    When the tenant admin performs crypto-shred for "org-pharma"
    Then tenant KEK "pharma-kek-001" is destroyed in the tenant KMS
    And all tenant KEK wrappings for "org-pharma" become invalid
    And system DEKs can no longer be unwrapped via tenant path
    And chunks remain on storage as system-encrypted ciphertext
    And refcounts for "org-pharma"'s references are decremented
    And the crypto-shred event is recorded in the audit log (system + tenant export)

  @integration
  Scenario: Crypto-shred with retention hold preserves ciphertext
    Given a retention hold "hipaa-7yr" is active on "org-pharma" namespace "trials"
    When crypto-shred is performed for "org-pharma"
    Then tenant KEK is destroyed (data unreadable)
    And chunks with refcount 0 are NOT physically deleted (hold active)
    And system-encrypted ciphertext is retained until hold expires
    And the hold-preserving-after-shred state is recorded in the audit log

  @integration
  Scenario: Crypto-shred does not affect other tenants' access
    Given chunk "shared-99" has refcount 2 (org-pharma and org-biotech, cross-tenant dedup)
    When "org-pharma" performs crypto-shred
    Then "org-pharma"'s KEK wrapping for "shared-99" is invalidated
    And "org-biotech"'s KEK wrapping remains valid
    And "org-biotech" can still read "shared-99"
    And "shared-99" refcount decrements to 1
    And "shared-99" is NOT eligible for GC (refcount > 0)

  # --- KMS connectivity ---

  @integration
  Scenario: Tenant KMS reachable from federated site
    Given "org-pharma" has data at site-EU and site-CH
    And tenant KMS is at "kms.pharma.internal"
    When site-CH needs to decrypt "org-pharma" data
    Then site-CH contacts "kms.pharma.internal" over encrypted channel
    And obtains tenant KEK wrapping for the requested system DEK
    And decryption proceeds using the unwrapped DEK
    And the KMS connection is authenticated and encrypted end-to-end

  # --- Key audit ---
  # @unit "All key lifecycle events are audited" moved to crate-level unit
  # test in crates/kiseki-keymanager/src/store.rs
  # (key_lifecycle_events_produce_structured_audit_data).
  # Verifies KeyLifecycleEvent structs carry timestamp, actor, key_id,
  # event_type, and tenant_id fields — without ever including key material.

  # --- Failure modes ---

  @integration
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

  @integration
  Scenario: Concurrent key rotation and crypto-shred
    Given "org-pharma" tenant admin initiates key rotation
    And simultaneously another admin initiates crypto-shred
    Then exactly one operation succeeds (serialized via tenant KMS)
    And if rotation wins: rotation completes, then shred can proceed with new KEK
    And if shred wins: KEK is destroyed, rotation is moot
    And the outcome is deterministic and audited

