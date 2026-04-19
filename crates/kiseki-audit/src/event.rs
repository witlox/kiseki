//! Audit event types.
//!
//! Every security-relevant operation emits an audit event. Events are
//! categorized by type and scoped to a tenant (or system-wide).

use kiseki_common::ids::{OrgId, SequenceNumber};
use kiseki_common::time::DeltaTimestamp;
use serde::{Deserialize, Serialize};

/// Audit event type categories.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum AuditEventType {
    // --- Key lifecycle ---
    /// System or tenant key generated.
    KeyGeneration,
    /// Key rotated (system KEK or tenant KEK).
    KeyRotation,
    /// Key destroyed (crypto-shred).
    KeyDestruction,
    /// Key accessed (system DEK unwrapped for read/write).
    KeyAccess,
    /// Full re-encryption triggered.
    ReEncryption,

    // --- Data access ---
    /// Data read by a tenant.
    DataRead,
    /// Data written by a tenant.
    DataWrite,
    /// Data deleted.
    DataDelete,

    // --- Authentication ---
    /// Successful authentication.
    AuthSuccess,
    /// Failed authentication attempt.
    AuthFailure,

    // --- Admin actions ---
    /// Tenant created/modified/deleted.
    TenantLifecycle,
    /// Cluster admin action.
    AdminAction,
    /// Policy change (quotas, compliance tags, etc.).
    PolicyChange,
    /// Maintenance mode entered/exited.
    MaintenanceMode,

    // --- Advisory (I-WA8) ---
    /// Workflow declared/ended.
    AdvisoryWorkflow,
    /// Hint accepted/rejected/throttled.
    AdvisoryHint,
    /// Budget exceeded.
    AdvisoryBudgetExceeded,
}

/// A single audit event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Sequence number within the audit shard (monotonic).
    pub sequence: SequenceNumber,
    /// When the event occurred.
    pub timestamp: DeltaTimestamp,
    /// Event type.
    pub event_type: AuditEventType,
    /// Tenant scope (None for system-wide events).
    pub tenant_id: Option<OrgId>,
    /// Actor: who performed the action.
    pub actor: String,
    /// Human-readable description of what happened.
    pub description: String,
}
