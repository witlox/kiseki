//! Identity and access management.
//!
//! Zero-trust boundary between cluster admin and tenant admin (I-T4):
//! cluster admin cannot access tenant config, logs, or data without
//! explicit tenant admin approval.
//!
//! Spec: `ubiquitous-language.md#Authentication`, I-T4, I-Auth4.

use std::time::{Duration, Instant};

use crate::error::ControlError;

/// What the access request covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccessScope {
    /// Access to a specific namespace.
    Namespace,
    /// Access to the entire tenant.
    Tenant,
}

/// Permitted operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccessLevel {
    /// Read operations only.
    ReadOnly,
    /// Read and write operations.
    ReadWrite,
}

/// Lifecycle state of an access request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestStatus {
    /// Awaiting tenant admin decision.
    Pending,
    /// Access granted for specified duration.
    Approved,
    /// Access request rejected.
    Denied,
    /// Access window has closed.
    Expired,
}

/// A cluster admin requesting access to tenant data (I-T4: deny by default).
#[derive(Clone, Debug)]
pub struct AccessRequest {
    /// Unique request identifier.
    pub id: String,
    /// Cluster admin identity.
    pub requester_id: String,
    /// Target tenant.
    pub tenant_id: String,
    /// Scope of access.
    pub scope: AccessScope,
    /// Namespace name (if scope is namespace).
    pub scope_target: String,
    /// Read-only or read-write.
    pub access_level: AccessLevel,
    /// Duration of the access window.
    pub duration_hours: u32,
    /// Current lifecycle state.
    pub status: RequestStatus,
    /// When access was granted (for expiry calculation).
    approved_at: Option<Instant>,
}

impl AccessRequest {
    /// Create a new pending access request.
    #[must_use]
    pub fn new(
        id: &str,
        requester_id: &str,
        tenant_id: &str,
        scope: AccessScope,
        scope_target: &str,
        access_level: AccessLevel,
        duration_hours: u32,
    ) -> Self {
        Self {
            id: id.to_owned(),
            requester_id: requester_id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            scope,
            scope_target: scope_target.to_owned(),
            access_level,
            duration_hours,
            status: RequestStatus::Pending,
            approved_at: None,
        }
    }

    /// Approve the request — transitions pending -> approved.
    pub fn approve(&mut self) -> Result<(), ControlError> {
        if self.status != RequestStatus::Pending {
            return Err(ControlError::Rejected(format!(
                "cannot approve request in status {:?}",
                self.status
            )));
        }
        self.status = RequestStatus::Approved;
        self.approved_at = Some(Instant::now());
        Ok(())
    }

    /// Deny the request — transitions pending -> denied.
    pub fn deny(&mut self) -> Result<(), ControlError> {
        if self.status != RequestStatus::Pending {
            return Err(ControlError::Rejected(format!(
                "cannot deny request in status {:?}",
                self.status
            )));
        }
        self.status = RequestStatus::Denied;
        Ok(())
    }

    /// Check whether the access grant is currently valid.
    #[must_use]
    pub fn is_active(&self) -> bool {
        if self.status != RequestStatus::Approved {
            return false;
        }
        if let Some(approved_at) = self.approved_at {
            let window = Duration::from_secs(u64::from(self.duration_hours) * 3600);
            Instant::now() < approved_at + window
        } else {
            false
        }
    }

    /// Check whether the requester can access the specified tenant data
    /// given the current request state.
    #[must_use]
    pub fn can_access(&self, tenant_id: &str) -> bool {
        self.tenant_id == tenant_id && self.is_active()
    }

    /// Build an audit event descriptor for this request.
    #[must_use]
    pub fn audit_event(&self) -> AuditEventDesc {
        AuditEventDesc {
            event_type: match self.status {
                RequestStatus::Pending => "access_request_created",
                RequestStatus::Approved => "access_request_approved",
                RequestStatus::Denied => "access_request_denied",
                RequestStatus::Expired => "access_request_expired",
            },
            requester_id: self.requester_id.clone(),
            tenant_id: self.tenant_id.clone(),
            scope_target: self.scope_target.clone(),
        }
    }
}

/// Audit event descriptor produced by IAM operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditEventDesc {
    /// Event type string.
    pub event_type: &'static str,
    /// Who requested access.
    pub requester_id: String,
    /// Target tenant.
    pub tenant_id: String,
    /// Scope target (namespace name, etc.).
    pub scope_target: String,
}

/// Check whether `acting_tenant_admin` can access `target_tenant_id`.
/// Returns `Ok(())` if they match, `Err` otherwise (full tenant isolation).
pub fn check_tenant_isolation(
    acting_tenant_id: &str,
    target_tenant_id: &str,
) -> Result<(), ControlError> {
    if acting_tenant_id == target_tenant_id {
        Ok(())
    } else {
        Err(ControlError::Rejected(format!(
            "tenant {acting_tenant_id} cannot access tenant {target_tenant_id}: full tenant isolation"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_request() -> AccessRequest {
        AccessRequest::new(
            "req-1",
            "admin-ops",
            "org-pharma",
            AccessScope::Namespace,
            "trials",
            AccessLevel::ReadOnly,
            4,
        )
    }

    #[test]
    fn cluster_admin_access_requires_approval() {
        // Scenario: Cluster admin requests access to tenant data - requires approval
        let req = pending_request();
        assert_eq!(req.status, RequestStatus::Pending);
        assert!(
            !req.can_access("org-pharma"),
            "pending request must not grant access"
        );
        let audit = req.audit_event();
        assert_eq!(audit.event_type, "access_request_created");
        assert_eq!(audit.requester_id, "admin-ops");
        assert_eq!(audit.tenant_id, "org-pharma");
    }

    #[test]
    fn approved_access_is_scoped_and_time_limited() {
        // Scenario: Cluster admin access request approved - scoped and time-limited
        let mut req = pending_request();
        req.approve().unwrap();
        assert_eq!(req.status, RequestStatus::Approved);
        assert!(req.is_active(), "just-approved request should be active");
        assert_eq!(req.scope, AccessScope::Namespace);
        assert_eq!(req.scope_target, "trials");
        assert_eq!(req.access_level, AccessLevel::ReadOnly);
        assert_eq!(req.duration_hours, 4);
        // Can access the right tenant
        assert!(req.can_access("org-pharma"));
        // Cannot access a different tenant
        assert!(!req.can_access("org-biotech"));
        let audit = req.audit_event();
        assert_eq!(audit.event_type, "access_request_approved");
    }

    #[test]
    fn denied_access_blocks_all_tenant_data() {
        // Scenario: Cluster admin access request denied
        let mut req = pending_request();
        req.deny().unwrap();
        assert_eq!(req.status, RequestStatus::Denied);
        assert!(
            !req.can_access("org-pharma"),
            "denied request must not grant access"
        );
        let audit = req.audit_event();
        assert_eq!(audit.event_type, "access_request_denied");
    }

    #[test]
    fn tenant_admin_cannot_access_other_tenant() {
        // Scenario: Tenant admin cannot access other tenant's data
        assert!(check_tenant_isolation("org-pharma", "org-pharma").is_ok());
        let err = check_tenant_isolation("org-pharma", "org-biotech");
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("tenant isolation"),
            "error should mention isolation: {msg}"
        );
    }
}
