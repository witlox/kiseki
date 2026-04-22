# ADR-028: External Tenant KMS Providers

**Status**: Accepted
**Date**: 2026-04-22
**Context**: I-K11, ADR-002, ADR-003, ADR-007
**Adversarial review**: 2026-04-22 (8 findings: 2H 5M 1L, all resolved)

## Problem

ADR-002 defines a two-layer encryption model where tenant KEKs wrap
access to system DEK derivation material. The current implementation
hardcodes tenant KEK as a locally-managed `[u8; 32]` — there is no
mechanism for tenants to bring their own key management infrastructure.

HPC and enterprise tenants require integration with their existing KMS:
- Regulatory compliance (FIPS 140-2/3, Common Criteria, SOC 2)
- Centralized key lifecycle management
- Hardware-backed key storage (HSMs)
- Audit trails in their own systems
- Key escrow and disaster recovery under their own policies

## Decision

Introduce a **`TenantKmsProvider` trait** with five backend
implementations. Tenant KEK sourcing becomes pluggable per-tenant
via control-plane configuration. The system key manager (ADR-007)
remains unchanged — only the tenant KEK layer is externalized.

### Provider Backends

| # | Backend | Type | Standard | Transport | Material model |
|---|---------|------|----------|-----------|----------------|
| 1 | **Kiseki Internal** | Built-in | — | In-process | Local |
| 2 | **HashiCorp Vault** | Open source | Proprietary (Transit) | HTTPS | Local (cached) |
| 3 | **KMIP 2.1** | Standard | OASIS KMIP SP 800-57 | mTLS (TTLV) | Remote or local |
| 4 | **AWS KMS** | Cloud | AWS Sig V4 | HTTPS | Remote only |
| 5 | **PKCS#11 v3.0** | HSM | OASIS PKCS#11 | Local (FFI) | Remote only (HSM) |

**Material model**: "Local" = KEK material cached in Kiseki process
memory. "Remote" = material never leaves the provider; all
wrap/unwrap operations are remote calls. The trait fully encapsulates
this distinction — callers never branch on provider type.

### Provider 1: Kiseki Internal (default)

The existing behavior. Kiseki manages tenant KEKs internally.
Suitable for deployments where tenants trust the operator or where
external KMS is unavailable.

- Tenant KEK generated internally on tenant creation
- Stored in a **separate Raft group** from system master keys
  (independent compromise domain — see Security Considerations §6)
- Rotation managed by Kiseki's epoch mechanism
- No external dependency

This is the **zero-configuration default**. Existing tenants and
single-operator deployments use this without change.

**Security trade-off**: Internal mode does not provide the full
two-layer security guarantee of ADR-002. A compromise of both the
system key manager and the tenant key store (even though they are
separate Raft groups) yields full access. Compliance-sensitive tenants
should use an external provider where the tenant KEK is under the
tenant's own operational control.

### Provider 2: HashiCorp Vault (Transit secrets engine)

Vault's Transit engine provides encryption-as-a-service with key
versioning that maps cleanly to Kiseki's epoch model.

**Operations mapping**:

| Kiseki operation | Vault API |
|-----------------|-----------|
| `wrap` | `POST /transit/encrypt/:name` (with `context` = AAD) |
| `unwrap` | `POST /transit/decrypt/:name` (with `context` = AAD) |
| `rotate` | `POST /transit/keys/:name/rotate` |
| `rewrap` | `POST /transit/rewrap/:name` (server-side, no plaintext exposure) |
| `destroy` | `DELETE /transit/keys/:name` (after enabling deletion) |

**Authentication methods** (tenant-configurable):
- **TLS certificate** — maps to Kiseki's SPIFFE/mTLS identity
- **AppRole** — role_id + secret_id for service authentication
- **Kubernetes** — ServiceAccount JWT (for k8s-deployed Kiseki)
- **OIDC/JWT** — external IdP token

**Vault namespaces**: Multi-tenant Vault deployments use namespaces to
isolate tenant key material. The tenant's Vault namespace is configured
at onboarding.

**Caching**: Vault provider may optionally cache KEK material locally
(fetched via `POST /transit/datakey/plaintext/:name`). When caching is
disabled, all wrap/unwrap calls go through Vault directly. Caching mode
is configurable per tenant.

**Rust crate**: `vaultrs` (maintained, async, supports Transit engine).

### Provider 3: KMIP 2.1 (OASIS standard)

KMIP is the interoperability standard for enterprise key management.
A single KMIP client covers: Thales CipherTrust Manager, IBM Security
Guardium Key Lifecycle Manager, Fortanix SDKMS, Entrust KeyControl,
NetApp StorageGRID KMS, Dell PowerProtect, and any KMIP-compliant HSM.

**Relevant OASIS specifications**:
- KMIP Specification v2.1 (2019) — protocol and operations
- KMIP Profiles v2.1 — conformance levels
- KMIP Usage Guide v2.1 — implementation guidance

**Operations mapping**:

| Kiseki operation | KMIP operation |
|-----------------|---------------|
| `wrap` | `Encrypt` with Correlation Value (AAD) |
| `unwrap` | `Decrypt` with Correlation Value (AAD) |
| `rotate` | `ReKey` or `Create` + `Activate` + `Revoke` old |
| `destroy` (crypto-shred) | `Destroy` (state → Destroyed, irrecoverable) |

**Transport**: TTLV (Tag-Type-Length-Value) binary encoding over
mTLS. The KMIP spec mandates mutual TLS with X.509 certificates.

**Key object attributes**: KMIP keys carry rich metadata —
`Cryptographic Algorithm`, `Cryptographic Length`, `State`
(Pre-Active/Active/Deactivated/Compromised/Destroyed),
`Activation Date`, `Deactivation Date`. These map to Kiseki's
`EpochInfo` (is_current, migration_complete).

**Material model**: Depends on KMIP server configuration. Some servers
allow `Get` to extract key material (local caching). Others enforce
non-extractable keys (remote-only wrap/unwrap). The provider detects
this via `CKA_EXTRACTABLE` equivalent attribute and adapts.

**Rust implementation**: No mature KMIP crate exists. Implement a
minimal KMIP client covering the Symmetric Key Foundry Client profile
(KMIP Profiles v2.1 §4.1). The wire format (TTLV) is straightforward
— ~1500 lines for the operations Kiseki needs.

### Provider 4: AWS KMS (cloud KMS exemplar)

AWS KMS as the reference cloud implementation. Azure Key Vault and
GCP Cloud KMS follow the same adapter pattern.

**Operations mapping**:

| Kiseki operation | AWS KMS API |
|-----------------|-------------|
| `wrap` | `Encrypt` (with `EncryptionContext` = AAD) |
| `unwrap` | `Decrypt` (with `EncryptionContext` = AAD) |
| `rotate` | `CreateKey` + `CreateAlias` (manual) or `EnableKeyRotation` (automatic annual) |
| `rewrap` | `ReEncrypt` (server-side, no plaintext exposure) |

**Key difference**: With cloud KMS, the KEK material **never leaves
the cloud provider**. Kiseki sends the derivation parameters (epoch +
chunk_id) to KMS for wrapping/unwrapping. This is strictly more secure
than local caching but adds network latency per operation.

**Caching strategy**: Kiseki caches the **unwrapped derivation
parameters** (not the KEK itself, which never leaves KMS). The
existing `KeyCache` TTL mechanism applies — after TTL expiry, a
new `Decrypt` call to KMS is required.

**Auth**: IAM role assumption via STS, instance metadata, or
environment credentials. For Azure: AAD/Managed Identity. For GCP:
service account key or Workload Identity.

**Rust crates**: `aws-sdk-kms`, `azure_security_keyvault`,
`google-cloud-kms` (all maintained, async).

### Provider 5: PKCS#11 v3.0 (HSM direct)

For tenants with on-premises HSMs (Thales Luna, Utimaco, nCipher,
YubiHSM). PKCS#11 is the standard C API for cryptographic tokens.

**Relevant standards**:
- OASIS PKCS#11 v3.0 (2020) — Cryptographic Token Interface
- PKCS#11 Profiles v3.0 — baseline/extended profiles

**Operations mapping**:

| Kiseki operation | PKCS#11 function |
|-----------------|-----------------|
| `wrap` | `C_WrapKey` (AES-KWP per RFC 5649, with `pParameter` = AAD) |
| `unwrap` | `C_UnwrapKey` |
| `rotate` | `C_GenerateKey` + `C_DestroyObject` (old, after migration) |
| `destroy` | `C_DestroyObject` |

**Material model**: Remote only. HSM keys are `CKA_SENSITIVE` and
`CKA_EXTRACTABLE=FALSE` by default — material never leaves the HSM.
All wrap/unwrap operations execute on the HSM hardware. Kiseki caches
unwrapped derivation parameters (same as cloud KMS model).

**Transport**: Local — PKCS#11 is a C shared library (`.so`/`.dylib`)
loaded via FFI. The HSM may be network-attached (e.g., Luna Network
HSM), but the PKCS#11 interface is local to the host.

**Rust crate**: `cryptoki` (maintained, wraps PKCS#11 C API).

## Trait Interface

```rust
/// Provider for tenant key encryption keys (KEKs).
///
/// Each tenant configures exactly one provider. The provider handles
/// authentication, key lifecycle, and wrapping/unwrapping operations.
/// The trait fully encapsulates the provider's material model — callers
/// never need to know whether wrapping happens locally or remotely.
///
/// Providers that cache KEK material locally (Internal, Vault) manage
/// their own cache internally. Providers where material never leaves
/// the backend (AWS KMS, PKCS#11) perform remote wrap/unwrap calls.
/// The caller's code path is identical in both cases.
#[async_trait]
pub trait TenantKmsProvider: Send + Sync {
    /// Wrap DEK derivation parameters (epoch + chunk_id) with the
    /// tenant KEK. The `aad` binds the wrapped ciphertext to its
    /// envelope context (typically chunk_id), preventing splice attacks.
    /// Returns opaque ciphertext stored in the envelope.
    async fn wrap(
        &self,
        tenant: &OrgId,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, KmsProviderError>;

    /// Unwrap DEK derivation parameters from envelope ciphertext.
    /// The `aad` must match the value used during wrapping.
    async fn unwrap(
        &self,
        tenant: &OrgId,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, KmsProviderError>;

    /// Rotate the tenant KEK to a new version/epoch.
    /// Returns the new provider-specific epoch identifier.
    async fn rotate(
        &self,
        tenant: &OrgId,
    ) -> Result<KmsEpochId, KmsProviderError>;

    /// Re-wrap ciphertext from old key version to current version
    /// without exposing plaintext (server-side re-wrap where supported).
    /// Falls back to unwrap + wrap if the provider doesn't support
    /// server-side re-wrap. The `aad` is preserved across the re-wrap.
    async fn rewrap(
        &self,
        tenant: &OrgId,
        old_ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, KmsProviderError>;

    /// Destroy the tenant KEK (crypto-shred). Irrecoverable.
    /// Also purges any locally cached material for this tenant.
    async fn destroy(
        &self,
        tenant: &OrgId,
    ) -> Result<(), KmsProviderError>;

    /// Check provider health and connectivity.
    async fn health(&self) -> KmsHealthStatus;

    /// Provider name for logging and diagnostics (never includes
    /// credentials or key material).
    fn provider_name(&self) -> &'static str;
}
```

**AAD usage**: Callers pass `chunk_id.as_bytes()` as `aad` for
per-chunk envelope wrapping. Each provider maps `aad` to its native
authenticated context mechanism:

| Provider | AAD mechanism |
|----------|-------------|
| Internal | AES-256-GCM additional data (existing `"kiseki-tenant-wrap-v1"` prefix + aad) |
| Vault | Transit `context` parameter (base64-encoded) |
| KMIP | `Correlation Value` attribute on Encrypt/Decrypt |
| AWS KMS | `EncryptionContext` key-value map (`{"chunk_id": "<hex>"}`) |
| PKCS#11 | `pParameter` field in mechanism struct |

## Tenant Configuration

Stored in the control plane (`kiseki-control`) per-tenant:

```rust
pub struct TenantKmsConfig {
    /// Provider type.
    pub provider: KmsProviderType,
    /// Provider-specific endpoint (URL, socket path, or "internal").
    pub endpoint: String,
    /// Authentication configuration. All secret fields use Zeroizing
    /// wrappers and implement Debug redaction (I-K8 extended).
    pub auth: KmsAuthConfig,
    /// Key identifier within the provider.
    pub key_name: String,
    /// Provider namespace (Vault namespace, KMIP group, KMS alias prefix).
    pub namespace: Option<String>,
    /// Cache TTL override (bounded by I-K15: 5s-300s).
    pub cache_ttl_secs: Option<u64>,
}

pub enum KmsProviderType {
    Internal,
    Vault,
    Kmip,
    AwsKms,
    AzureKeyVault,
    GcpCloudKms,
    Pkcs11,
}

/// Authentication configuration for external KMS providers.
///
/// All secret fields use `Zeroizing<String>` for automatic memory
/// clearing on drop. The `Debug` impl prints variant names only —
/// never credential contents (I-K8 extended to provider credentials).
pub enum KmsAuthConfig {
    /// Internal provider — no external auth needed.
    None,
    /// mTLS client certificate (KMIP, Vault TLS auth).
    TlsCert {
        cert_pem: String,
        key_pem: Zeroizing<String>,
    },
    /// Vault AppRole.
    AppRole {
        role_id: String,
        secret_id: Zeroizing<String>,
    },
    /// OIDC/JWT token (Vault, cloud providers).
    Oidc {
        token_endpoint: String,
        client_id: String,
    },
    /// AWS IAM role assumption.
    AwsIamRole {
        role_arn: String,
        region: String,
    },
    /// Azure Managed Identity or Service Principal.
    AzureIdentity {
        tenant_id: String,
        client_id: String,
    },
    /// GCP Service Account.
    GcpServiceAccount {
        credentials_json: Zeroizing<String>,
    },
    /// PKCS#11 library path + slot/pin.
    Pkcs11 {
        library_path: String,
        slot_id: u64,
        pin: Zeroizing<String>,
    },
}
```

**I-K8 extended**: `KmsAuthConfig` implements `Debug` with redaction:
```rust
impl fmt::Debug for KmsAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "KmsAuthConfig::None"),
            Self::TlsCert { .. } => write!(f, "KmsAuthConfig::TlsCert(***)"),
            Self::AppRole { role_id, .. } => write!(f, "KmsAuthConfig::AppRole({})", role_id),
            // ... all variants redact secret fields
        }
    }
}
```

## Caching and Fallback

The existing `KeyCache` (cache.rs) is reused for providers with local
material. Remote-only providers (AWS KMS, PKCS#11) cache unwrapped
derivation parameters instead.

| Provider | What is cached | Cache miss action |
|----------|---------------|-------------------|
| Internal | KEK material (32 bytes) | Fetch from tenant key Raft store |
| Vault | KEK material or nothing (configurable) | `POST /transit/decrypt` |
| KMIP | KEK material or nothing (depends on server) | `Encrypt`/`Decrypt` operation |
| AWS KMS | Unwrapped derivation params | `Decrypt` API call |
| PKCS#11 | Unwrapped derivation params | `C_UnwrapKey` |

**I-K15 applies**: Cache TTL bounded to [5s, 300s] regardless of
provider. This ensures crypto-shred takes effect within the TTL
window even if the external KMS is ahead of Kiseki's cache.

**Provider unavailability**:
- Within TTL window: cached material serves reads (degraded mode)
- Beyond TTL: reads fail with `TenantKekUnavailable` (retriable)
- Writes always require fresh validation (no stale-cache writes)

**Resilience** (adversarial finding #5):
- **Circuit breaker** per provider endpoint: open after 5 consecutive
  failures/timeouts, half-open probe every 30s
- **Jittered cache TTL**: actual TTL = configured TTL ± 10% (random)
  to prevent synchronized expiry across storage nodes
- **Concurrency limit**: max 10 concurrent KMS requests per tenant
  per storage node (backpressure, not queuing)
- **Timeout bounds**: 2s connect timeout, 5s operation timeout for
  all network-based providers

**I-K11 unchanged**: Kiseki provides no escrow. If the tenant loses
access to their external KMS and has no backup, their data is
unrecoverable. This is documented and accepted.

## Provider Migration

Changing a tenant's KMS provider (e.g., Internal → Vault) requires
re-wrapping all existing envelopes (adversarial finding #3):

1. Provision new KEK in the target provider
2. Configure the new provider as "pending" in control plane
3. Background re-wrap: for each envelope, `old_provider.unwrap()` →
   `new_provider.wrap()` with the same AAD
4. Track progress (same mechanism as epoch re-wrap: `RewrapProgress`)
5. Once 100% re-wrapped, atomically switch active provider
6. Decommission old provider KEK

During migration, reads use whichever provider matches the envelope's
`tenant_epoch`. The envelope carries a provider-version tag to
disambiguate.

**Constraint**: Provider migration is an operator-initiated,
audited action. It cannot be triggered by the tenant API alone.

## Crypto-Shred Interaction

Crypto-shred (tenant KEK destruction) behavior per provider:

| Provider | Crypto-shred mechanism |
|----------|----------------------|
| Internal | Delete KEK from tenant key store; purge cache |
| Vault | `POST /transit/keys/:name/config` with `deletion_allowed=true`, then `DELETE /transit/keys/:name` |
| KMIP | `Destroy` operation (state → Destroyed, irrecoverable) |
| AWS KMS | `DisableKey` (immediate, blocks all operations) + `ScheduleKeyDeletion` (permanent, 7-30 day window) |
| PKCS#11 | `C_DestroyObject` |

**AWS KMS**: `DisableKey` is called immediately on crypto-shred to
block all wrap/unwrap operations. `ScheduleKeyDeletion` follows for
permanent destruction. The 7-day AWS-enforced waiting period applies
to permanent deletion only — the key is operationally dead from the
moment `DisableKey` is called. The `health()` check reports
`supports_immediate_shred: true` (via DisableKey) so tenants can
verify crypto-shred SLA compliance at configuration time.

## Security Considerations

1. **Credential protection**: KMS auth credentials stored in the
   control plane are encrypted at rest with the system master key.
   All secret fields use `Zeroizing<String>` for memory protection.
   `Debug` implementations redact all credential content (I-K8
   extended). Credentials are excluded from core dumps via
   `MADV_DONTDUMP` on the containing allocation.

2. **Network isolation**: External KMS calls are made from storage
   nodes, not the control plane. This avoids routing tenant data
   through the control plane. mTLS is required for all network-based
   providers.

3. **Provider compromise**: If a tenant's external KMS is compromised,
   only that tenant's data is at risk. System master keys and other
   tenants are unaffected (tenant isolation, I-T3).

4. **Mixed providers**: Different tenants can use different providers.
   A single Kiseki cluster can serve tenants using Vault, AWS KMS,
   and internal management simultaneously.

5. **FIPS compliance**: The HKDF derivation and AES-256-GCM encryption
   remain on Kiseki's FIPS-validated aws-lc-rs module regardless of
   provider. The external KMS only handles the tenant KEK wrapping
   layer — the system encryption layer is always FIPS.

6. **Internal provider isolation**: Tenant KEKs in Internal mode are
   stored in a **separate Raft group** from system master keys. This
   provides an independent compromise domain — system key manager
   compromise alone does not yield tenant KEKs, and vice versa.
   However, an operator with access to both stores has full access.
   Compliance-sensitive tenants should use an external provider where
   the KEK is under their own operational control.

## Implementation Phases

1. **Phase K1**: `TenantKmsProvider` trait + Internal backend (refactor
   current code to use the trait; no behavioral change)
2. **Phase K2**: Vault backend (Transit engine, `vaultrs` crate)
3. **Phase K3**: KMIP 2.1 backend (custom TTLV client, ~1500 lines)
4. **Phase K4**: AWS KMS backend (`aws-sdk-kms` crate)
5. **Phase K5**: PKCS#11 backend (`cryptoki` crate)

Phases K2-K5 are independent and can be built in any order.

## Alternatives Considered

1. **BYOK (Bring Your Own Key) upload model**: Tenant uploads raw key
   material to Kiseki. Rejected — defeats the purpose of external KMS
   (key material leaves tenant's control boundary).

2. **Single cloud KMS only**: Support only AWS KMS. Rejected — HPC
   customers are frequently on-premises or multi-cloud.

3. **KMIP only**: Use KMIP as the sole external standard. Rejected —
   Vault and cloud KMS are too prevalent to ignore, and KMIP client
   implementation cost is non-trivial.

4. **No internal provider**: Require all tenants to configure external
   KMS. Rejected — creates unnecessary deployment friction for simple
   or single-operator clusters.

5. **`fetch_kek` in trait interface**: Original design included
   `fetch_kek() -> Option<TenantKekMaterial>` with `None` for cloud
   providers. Rejected after adversarial review — leaky abstraction
   that forces callers to branch on provider model. `wrap`/`unwrap`
   as the universal interface fully encapsulates the distinction.

## Adversarial Review Findings (2026-04-22)

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | High | Credential fields as plaintext `String` | `Zeroizing<String>` + Debug redaction |
| 2 | High | `fetch_kek` leaky abstraction | Removed; `wrap`/`unwrap` are universal |
| 3 | Medium | No provider migration path | Migration protocol documented |
| 4 | Medium | No AAD in wrap/unwrap | `aad: &[u8]` parameter added |
| 5 | Medium | No rate limiting/circuit breaker | Circuit breaker + jitter + limits specified |
| 6 | Medium | PKCS#11 `C_GetAttributeValue` violates HSM model | Removed; HSM uses `C_WrapKey`/`C_UnwrapKey` only |
| 7 | Medium | Internal KEK co-located with system keys | Separate Raft group for tenant KEKs |
| 8 | Low | AWS KMS 7-day deletion window | `DisableKey` immediate + `ScheduleKeyDeletion` deferred |

## Consequences

- Adds `kiseki-kms` crate (or module within `kiseki-keymanager`)
- Tenant key Raft group added (separate from system key manager)
- Control plane gains KMS configuration endpoints
- Each storage node needs network access to tenant KMS endpoints
- KMIP requires custom protocol implementation (~1500 lines)
- PKCS#11 requires unsafe FFI (contained within cryptoki crate)
- Testing requires mock KMS servers (Vault dev mode, LocalStack,
  SoftHSM for PKCS#11)
