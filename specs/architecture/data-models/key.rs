//! Key Management context types — system key manager, tenant KMS integration.
//! Spec: domain-model.md#KeyManagement, features/key-management.feature

use crate::common::*;
use crate::crypto::*;

// --- System key manager state ---

/// System key manager — internal HA service.
/// Spec: I-K12, ubiquitous-language.md#SystemKeyManager
pub struct KeyManagerState {
    /// Current system master key epoch
    pub current_epoch: KeyEpoch,
    /// Previous epochs retained during rotation window
    pub retained_epochs: Vec<KeyEpoch>,
    /// Raft group for HA
    pub raft_members: Vec<NodeId>,
    pub leader: Option<NodeId>,
}

// --- Key rotation ---

/// Spec: ubiquitous-language.md#KeyEpoch, I-K6
pub struct KeyRotationState {
    pub old_epoch: KeyEpoch,
    pub new_epoch: KeyEpoch,
    pub started_at: DeltaTimestamp,
    /// Progress of background re-wrapping
    pub progress: RotationProgress,
}

pub enum RotationProgress {
    /// New writes use new epoch; old data untouched
    InProgress { migrated_pct: f32 },
    /// All data migrated to new epoch
    Complete,
    /// Full re-encryption (admin action for key compromise)
    FullReEncryption { re_encrypted_pct: f32 },
}

// --- Crypto-shred ---

/// Spec: ubiquitous-language.md#CryptoShred, I-K5
pub struct CryptoShredRequest {
    pub tenant_id: OrgId,
    /// Must verify retention holds are in place before proceeding
    pub force_without_hold_check: bool,
}

pub struct CryptoShredResult {
    pub tenant_id: OrgId,
    pub kek_destroyed: bool,
    pub compositions_affected: u64,
    pub chunks_refcount_decremented: u64,
    pub chunks_gc_eligible: u64,
    pub chunks_held_by_retention: u64,
    pub timestamp: DeltaTimestamp,
}

// --- Tenant KMS integration ---

/// Configuration for connecting to a tenant's external KMS.
pub struct TenantKmsConfig {
    pub tenant_id: OrgId,
    /// Endpoint (e.g., "kms.pharma.internal:443")
    pub endpoint: String,
    /// Authentication method for KMS connection
    pub auth: KmsAuthMethod,
    /// Cache TTL for tenant KEK material
    pub cache_ttl_seconds: u32,
}

pub enum KmsAuthMethod {
    MtlsCertificate { cert_path: String, key_path: String },
    Token { token: String },
    AwsKms { region: String, key_arn: String },
    Vault { endpoint: String, role: String },
}

// --- Commands ---

pub struct DeriveSystemDekRequest {
    pub chunk_id: ChunkId,
    pub epoch: KeyEpoch,
}

pub struct RotateSystemKeyRequest {
    /// Admin-triggered
    pub triggered_by: String,
}

pub struct RotateTenantKeyRequest {
    pub tenant_id: OrgId,
    pub triggered_by: String,
}

pub struct FullReEncryptRequest {
    pub tenant_id: OrgId,
    pub reason: String,
}

pub struct UnwrapTenantKeyRequest {
    pub tenant_id: OrgId,
    pub epoch: KeyEpoch,
}

// --- Audit events ---

/// Key lifecycle events — all recorded in audit log.
/// Spec: I-A1, features/key-management.feature#AuditScenario
pub enum KeyAuditEvent {
    SystemKeyGenerated { epoch: KeyEpoch },
    SystemKeyRotated { old_epoch: KeyEpoch, new_epoch: KeyEpoch },
    TenantKeyRotated { tenant_id: OrgId, old_epoch: KeyEpoch, new_epoch: KeyEpoch },
    CryptoShredExecuted(CryptoShredResult),
    FullReEncryptStarted { tenant_id: OrgId, reason: String },
    FullReEncryptCompleted { tenant_id: OrgId },
    KeyAccessed { tenant_id: OrgId, epoch: KeyEpoch, purpose: String },
}

// --- Trait stubs ---

/// System key manager operations.
pub trait KeyManagerOps {
    /// Derive a system DEK for a given chunk (HKDF from master key).
    /// ADR-003: derived, not stored individually.
    fn derive_system_dek(&self, req: DeriveSystemDekRequest) -> Result<SystemDek, KisekiError>;

    fn rotate_system_key(&self, req: RotateSystemKeyRequest) -> Result<KeyEpoch, KisekiError>;
    fn key_manager_health(&self) -> Result<KeyManagerState, KisekiError>;
}

/// Tenant KMS integration operations.
pub trait TenantKmsOps {
    /// Fetch and cache tenant KEK from external KMS.
    fn fetch_tenant_kek(&self, tenant_id: OrgId, epoch: KeyEpoch) -> Result<TenantKek, KisekiError>;

    /// Rotate tenant key (triggers epoch change in tenant KMS).
    fn rotate_tenant_key(&self, req: RotateTenantKeyRequest) -> Result<KeyEpoch, KisekiError>;

    /// Execute crypto-shred — destroy tenant KEK.
    fn crypto_shred(&self, req: CryptoShredRequest) -> Result<CryptoShredResult, KisekiError>;

    /// Trigger full re-encryption (key compromise response).
    fn full_re_encrypt(&self, req: FullReEncryptRequest) -> Result<(), KisekiError>;

    /// Check tenant KMS reachability.
    fn check_kms_health(&self, tenant_id: OrgId) -> Result<bool, KisekiError>;
}
