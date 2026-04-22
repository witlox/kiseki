//! Per-method authorization for `ControlService` gRPC (ADV-S3).
//!
//! Classifies each RPC method as tenant-scoped or admin-only.
//! Admin-only methods require the caller to have cluster admin role
//! (extracted from mTLS cert OU or SPIFFE SAN by the server runtime).

use tonic::{Request, Status};

/// Caller role extracted from mTLS certificate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallerRole {
    /// Cluster administrator — can call all methods.
    ClusterAdmin,
    /// Tenant administrator — can call tenant-scoped methods only.
    TenantAdmin,
    /// Unknown / unauthenticated (plaintext dev mode).
    Unknown,
}

/// Methods that require cluster admin role.
const ADMIN_ONLY_METHODS: &[&str] = &[
    "set_maintenance_mode",
    "register_peer",
    "list_peers",
    "set_retention_hold",
    "release_retention_hold",
    "set_compliance_tags",
    "set_quota",
    "list_flavors",
    "match_flavor",
];

/// Check if a method name requires admin authorization.
#[must_use]
pub fn is_admin_only(method: &str) -> bool {
    ADMIN_ONLY_METHODS.contains(&method)
}

/// Extract caller role from gRPC request metadata.
///
/// In production, the server's tonic interceptor sets the `x-kiseki-role`
/// metadata key from the mTLS certificate's OU field. In dev mode
/// (plaintext), the role defaults to `ClusterAdmin` for convenience.
pub fn extract_role<T>(req: &Request<T>) -> CallerRole {
    if let Some(role_val) = req.metadata().get("x-kiseki-role") {
        match role_val.to_str().unwrap_or("") {
            "cluster-admin" => CallerRole::ClusterAdmin,
            "tenant-admin" => CallerRole::TenantAdmin,
            _ => CallerRole::Unknown,
        }
    } else {
        // No role metadata — dev mode, assume admin.
        CallerRole::ClusterAdmin
    }
}

/// Require admin role or return `PERMISSION_DENIED`.
pub fn require_admin<T>(req: &Request<T>) -> Result<(), Status> {
    let role = extract_role(req);
    if role == CallerRole::ClusterAdmin {
        Ok(())
    } else {
        Err(Status::permission_denied(
            "this operation requires cluster admin role",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataValue;

    #[test]
    fn admin_only_methods_classified() {
        assert!(is_admin_only("set_maintenance_mode"));
        assert!(is_admin_only("register_peer"));
        assert!(!is_admin_only("create_organization"));
        assert!(!is_admin_only("create_project"));
    }

    #[test]
    fn require_admin_rejects_tenant() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-kiseki-role", MetadataValue::from_static("tenant-admin"));
        assert!(require_admin(&req).is_err());
    }

    #[test]
    fn require_admin_accepts_cluster_admin() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-kiseki-role", MetadataValue::from_static("cluster-admin"));
        assert!(require_admin(&req).is_ok());
    }

    #[test]
    fn no_role_defaults_to_admin_in_dev() {
        let req = Request::new(());
        // No metadata — dev mode, should pass.
        assert!(require_admin(&req).is_ok());
    }
}
