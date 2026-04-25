Feature: External Tenant KMS Providers (ADR-028)

  Pluggable tenant KEK sourcing via external key management systems.
  Five providers: Kiseki Internal, HashiCorp Vault, KMIP 2.1,
  AWS KMS, PKCS#11. Provider selection is per-tenant. The trait
  encapsulates local-vs-remote material models — callers never branch.

  Background:
    Given a Kiseki cluster with a system key manager
    And system master key in epoch 1

  # === Provider configuration ===

  @integration
  Scenario: Tenant configures Vault provider
    When tenant "org-pharma" configures KMS:
      | field     | value                              |
      | provider  | vault                              |
      | endpoint  | https://vault.pharma.internal:8200 |
      | auth      | approle                            |
      | key_name  | kiseki-pharma-kek                  |
      | namespace | pharma/production                  |
    Then the provider is validated (health check passes)
    And a test wrap/unwrap round-trip succeeds
    And the configuration is stored in the control plane
    And the configuration event is recorded in the audit log

  @integration
  Scenario: Tenant configures KMIP 2.1 provider
    When tenant "org-defense" configures KMS:
      | field     | value                              |
      | provider  | kmip                               |
      | endpoint  | kmip.defense.gov:5696              |
      | auth      | tls-cert                           |
      | key_name  | kiseki-defense-kek                 |
    Then the provider connects via mTLS with TTLV encoding
    And the KMIP server's Symmetric Key object is located
    And a test wrap/unwrap round-trip succeeds

  @integration
  Scenario: Tenant configures AWS KMS provider
    When tenant "org-cloud" configures KMS:
      | field     | value                              |
      | provider  | aws-kms                            |
      | endpoint  | us-east-1                          |
      | auth      | iam-role                           |
      | key_name  | alias/kiseki-cloud-kek             |
    Then the provider authenticates via IAM role assumption
    And a test wrap/unwrap round-trip succeeds
    And KEK material never leaves the AWS KMS boundary

  @integration
  Scenario: Tenant configures PKCS#11 HSM provider
    When tenant "org-bank" configures KMS:
      | field        | value                           |
      | provider     | pkcs11                          |
      | library_path | /usr/lib/libCryptoki2.so        |
      | slot_id      | 0                               |
      | key_name     | kiseki-bank-kek                 |
    Then the PKCS#11 library is loaded via FFI
    And the HSM key handle is resolved via C_FindObjects with label "kiseki-bank-kek"
    And a test wrap/unwrap round-trip succeeds via C_WrapKey/C_UnwrapKey
    And key material never leaves the HSM

  # === Wrap/unwrap operations ===

  @integration
  Scenario: HSM unwrap — material stays in hardware
    Given tenant "org-bank" with PKCS#11 provider
    When a chunk is read
    Then C_UnwrapKey is called on the HSM
    And the HSM performs the unwrap internally
    And only the unwrapped derivation parameters cross the PKCS#11 boundary
    And KEK material never exists in host memory

  # === Internal provider ===

  @integration
  Scenario: Internal provider KEK isolation from system master keys
    Given tenant "org-internal" with Internal KMS provider
    When the tenant KEK is generated
    Then it is stored in the tenant key Raft group
    And NOT in the system key manager Raft group
    And the two Raft groups are independent failure domains
    And compromise of the system key manager alone does not expose tenant KEKs

  # === Caching ===

  @unit
  Scenario: Cache TTL expiry triggers provider fetch
    Given tenant "org-pharma" with Vault KMS provider
    And the KEK was cached 310 seconds ago with TTL 300 seconds
    When a read request arrives
    Then a new unwrap call is made to Vault
    And the cache is refreshed
    And the read succeeds

  @unit
  Scenario: Cache TTL jitter prevents thundering herd
    Given 100 storage nodes caching tenant "org-pharma" KEK with TTL 60 seconds
    Then actual TTL per node is 60 +/- 10% (54s to 66s, randomized)
    And cache misses are spread across a 12-second window
    And no synchronized burst of KMS requests occurs

  # === Provider resilience ===

  @unit
  Scenario: Circuit breaker opens after consecutive failures
    Given tenant "org-pharma" with Vault KMS provider
    When 5 consecutive wrap/unwrap calls fail with timeout
    Then the circuit breaker opens for "org-pharma" provider
    And subsequent calls fail immediately with "circuit open" error
    And a half-open probe is sent every 30 seconds
    And when the probe succeeds, the circuit closes
    And operations resume normally

  @unit
  Scenario: Concurrency limit prevents KMS overload
    Given tenant "org-pharma" with Vault KMS provider
    And max concurrent KMS requests is 10 per storage node
    When 20 simultaneous unwrap requests arrive
    Then 10 are dispatched to Vault
    And 10 receive backpressure ("KMS concurrency limit reached")
    And no more than 10 connections are open to Vault simultaneously

  @unit
  Scenario: Provider timeout bounds enforced
    Given tenant "org-pharma" with Vault KMS provider
    When Vault takes 6 seconds to respond to an unwrap call
    Then the call times out at 5 seconds (operation timeout)
    And the read fails with retriable "KMS timeout" error
    And the timeout counts toward the circuit breaker threshold

  # === Key rotation via provider ===

  @integration
  Scenario: Vault provider key rotation
    Given tenant "org-pharma" with Vault KMS provider
    When the tenant admin triggers key rotation
    Then Vault Transit key is rotated (POST /transit/keys/:name/rotate)
    And new wraps use the latest key version
    And background re-wrap migrates old envelopes via Vault rewrap API
    And old envelopes remain readable during migration
    And the rotation event is recorded in the audit log

  @integration
  Scenario: AWS KMS provider key rotation
    Given tenant "org-cloud" with AWS KMS provider
    When the tenant admin triggers key rotation
    Then a new KMS key is created (or auto-rotation fires)
    And new wraps use the new key
    And background re-wrap uses ReEncrypt (server-side, no plaintext)
    And old envelopes remain readable during migration

  @integration
  Scenario: PKCS#11 provider key rotation
    Given tenant "org-bank" with PKCS#11 provider
    When the tenant admin triggers key rotation
    Then C_GenerateKey creates a new AES-256 key on the HSM
    And new wraps use the new key handle
    And background re-wrap: C_UnwrapKey (old) then C_WrapKey (new)
    And old key object is retained until migration completes
    And C_DestroyObject removes the old key after migration

  # === Crypto-shred per provider ===

  @integration
  Scenario: Internal provider crypto-shred
    Given tenant "org-internal" with Internal KMS provider
    When crypto-shred is performed
    Then the tenant KEK is deleted from the tenant key Raft group
    And the local cache is purged immediately
    And all tenant data becomes unreadable
    And the shred event is recorded in the audit log

  @integration
  Scenario: Vault provider crypto-shred
    Given tenant "org-pharma" with Vault KMS provider
    When crypto-shred is performed
    Then Vault key deletion is enabled (deletion_allowed=true)
    And the Transit key is deleted (DELETE /transit/keys/:name)
    And the local cache is purged immediately
    And all tenant data becomes unreadable

  @integration
  Scenario: AWS KMS crypto-shred — immediate disable + deferred delete
    Given tenant "org-cloud" with AWS KMS provider
    When crypto-shred is performed
    Then DisableKey is called immediately (blocks all operations)
    And ScheduleKeyDeletion is called (7-day AWS-enforced wait)
    And the local cache is purged immediately
    And all tenant data becomes unreadable from the moment DisableKey fires
    And the 7-day window is for permanent deletion only (key is already dead)

  @integration
  Scenario: KMIP provider crypto-shred
    Given tenant "org-defense" with KMIP 2.1 provider
    When crypto-shred is performed
    Then KMIP Destroy operation is sent
    And the key state transitions to "Destroyed" (irrecoverable)
    And the local cache is purged immediately

  @integration
  Scenario: PKCS#11 provider crypto-shred
    Given tenant "org-bank" with PKCS#11 provider
    When crypto-shred is performed
    Then C_DestroyObject is called on the HSM
    And the key is permanently erased from hardware
    And the local cache is purged immediately

  # === Provider migration ===

  @integration
  Scenario: Migrate from Internal to Vault provider
    Given tenant "org-growing" with Internal KMS provider
    And 1000 chunks exist with Internal-wrapped envelopes
    When the operator initiates provider migration to Vault:
      | field     | value                              |
      | provider  | vault                              |
      | endpoint  | https://vault.growing.io:8200      |
      | key_name  | kiseki-growing-kek                 |
    Then the new Vault provider is configured as "pending"
    And a new KEK is provisioned in Vault
    And background re-wrap begins: unwrap(Internal) then wrap(Vault) per envelope
    And progress is tracked (0/1000, then 500/1000, then 1000/1000)
    And reads use whichever provider matches the envelope's provider tag
    And when 100% re-wrapped, the active provider switches to Vault atomically
    And the old Internal KEK is decommissioned

  @integration
  Scenario: Provider migration preserves data availability
    Given tenant "org-growing" migration from Internal to Vault is at 50%
    When a read arrives for a chunk still wrapped with Internal provider
    Then the Internal provider unwraps it successfully
    When a read arrives for a chunk already wrapped with Vault provider
    Then the Vault provider unwraps it successfully
    And both providers are active during migration

  # === Credential security ===

  @unit
  Scenario: KMS credentials encrypted at rest
    Given tenant "org-pharma" with Vault KMS provider
    And AppRole secret_id "s.abc123" configured
    Then the secret_id is encrypted with the system master key in the control plane
    And the secret_id is stored as Zeroizing<String> in memory
    And the secret_id never appears in logs, debug output, or core dumps

  @unit
  Scenario: KMS credential Debug output is redacted
    Given tenant "org-pharma" with AppRole auth configuration
    When the KmsAuthConfig is formatted for debug logging
    Then the output is "KmsAuthConfig::AppRole(role-id-123)"
    And the secret_id is replaced with "***"
    And no credential material appears in the log

  # === Mixed provider cluster ===

  @integration
  Scenario: Three tenants with three different providers
    Given tenant "org-alpha" with Internal KMS provider
    And tenant "org-beta" with Vault KMS provider
    And tenant "org-gamma" with AWS KMS provider
    When all three tenants write and read data concurrently
    Then each tenant's wrap/unwrap uses its configured provider
    And no cross-tenant provider interference occurs
    And a provider failure for "org-beta" does not affect "org-alpha" or "org-gamma"

  # === Security edge cases ===

  @unit
  Scenario: Internal provider — operator access trade-off documented
    Given tenant "org-internal" with Internal KMS provider
    Then the tenant is informed at configuration time:
      | warning | Internal mode does not provide full two-layer security |
      | reason  | Operator with access to both Raft groups has full access |
      | recommendation | Compliance-sensitive tenants should use external provider |
    And this trade-off is recorded in the tenant's configuration metadata

  # === Additional security and operational edge cases ===

  @unit
  Scenario: KMS credential rotation does not leak old secrets
    Given tenant "org-pharma" with Vault AppRole auth
    When the secret_id is rotated to a new value
    Then the old secret_id is zeroized from memory
    And the old secret_id does not appear in logs

  @integration
  Scenario: Provider migration can be cancelled mid-operation
    Given migration from Internal to Vault at 50%
    When the operator cancels the migration
    Then re-wrapped envelopes revert to Internal provider
    And no data is lost
