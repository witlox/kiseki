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
}
