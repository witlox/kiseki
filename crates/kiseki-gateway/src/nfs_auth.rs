//! NFS authentication for Kiseki gateway (WS 5.3).
//!
//! Validates NFS client credentials against per-export configuration,
//! mapping `AUTH_SYS` UIDs or Kerberos principals to tenant identity.
//!
//! Invariant mapping:
//!   - I-Auth1 — mTLS on data fabric connections
//!   - I-Auth2 — optional tenant `IdP` second-stage auth

use kiseki_common::ids::OrgId;

/// NFS authentication method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NfsAuthMethod {
    /// `AUTH_SYS` (traditional Unix UID/GID).
    AuthSys,
    /// `RPCSEC_GSS` with Kerberos v5.
    Kerberos,
    /// No authentication (rejected in production).
    None,
}

/// Parsed NFS auth credentials.
#[derive(Clone, Debug)]
pub struct NfsCredentials {
    /// Authentication method used by the client.
    pub method: NfsAuthMethod,
    /// Unix user ID.
    pub uid: u32,
    /// Unix group ID.
    pub gid: u32,
    /// Client hostname.
    pub hostname: String,
    /// Kerberos principal if `RPCSEC_GSS`.
    pub principal: Option<String>,
}

impl NfsCredentials {
    /// Construct credentials from `AUTH_SYS` parameters.
    #[must_use]
    pub fn from_auth_sys(uid: u32, gid: u32, hostname: String) -> Self {
        Self {
            method: NfsAuthMethod::AuthSys,
            uid,
            gid,
            hostname,
            principal: None,
        }
    }
}

/// NFS auth configuration per export.
pub struct NfsExportAuth {
    /// Export path (e.g., "/data/project-a").
    pub path: String,
    /// Authentication methods allowed for this export.
    pub allowed_methods: Vec<NfsAuthMethod>,
    /// Tenant that owns this export.
    pub tenant_id: OrgId,
    /// How UIDs map to tenant identity.
    pub uid_mapping: UidMapping,
}

impl NfsExportAuth {
    /// Whether `method` is permitted on this export.
    #[must_use]
    pub fn allows_method(&self, method: NfsAuthMethod) -> bool {
        self.allowed_methods.contains(&method)
    }
}

/// How UIDs map to tenant identity.
#[derive(Clone, Debug)]
pub enum UidMapping {
    /// All UIDs map to the same tenant (simple).
    AllToTenant,
    /// UID ranges map to different projects.
    RangeMapping(Vec<UidRange>),
}

/// A UID range mapped to a project.
#[derive(Clone, Debug)]
pub struct UidRange {
    /// Start of range (inclusive).
    pub start: u32,
    /// End of range (inclusive).
    pub end: u32,
    /// Project identifier for UIDs in this range.
    pub project_id: String,
}

/// NFS auth error.
#[derive(Debug, thiserror::Error)]
pub enum NfsAuthError {
    /// The client's auth method is not allowed on this export.
    #[error("auth method not allowed: {0:?}")]
    MethodNotAllowed(NfsAuthMethod),
    /// No credentials were provided.
    #[error("no credentials provided")]
    NoCredentials,
    /// Kerberos principal not found or invalid.
    #[error("kerberos principal not found: {0}")]
    PrincipalNotFound(String),
    /// UID does not fall within any allowed range.
    #[error("uid not in any allowed range: {0}")]
    UidNotMapped(u32),
}

/// Validate NFS credentials against an export's auth configuration.
///
/// Returns the tenant `OrgId` on success.
///
/// # Errors
///
/// Returns `NfsAuthError` when the auth method is not allowed, the UID
/// is not in a mapped range, or a Kerberos principal is missing.
pub fn validate_credentials(
    creds: &NfsCredentials,
    export: &NfsExportAuth,
) -> Result<OrgId, NfsAuthError> {
    // Reject disallowed auth methods.
    if !export.allows_method(creds.method) {
        return Err(NfsAuthError::MethodNotAllowed(creds.method));
    }

    // Kerberos requires a principal.
    if creds.method == NfsAuthMethod::Kerberos {
        let principal = creds
            .principal
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| NfsAuthError::PrincipalNotFound(creds.hostname.clone()))?;
        // Principal exists — in a full implementation we'd validate
        // against the KDC. For now, presence is sufficient.
        let _ = principal;
    }

    // Validate UID mapping.
    match &export.uid_mapping {
        UidMapping::AllToTenant => Ok(export.tenant_id),
        UidMapping::RangeMapping(ranges) => {
            for range in ranges {
                if creds.uid >= range.start && creds.uid <= range.end {
                    return Ok(export.tenant_id);
                }
            }
            Err(NfsAuthError::UidNotMapped(creds.uid))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_org_id() -> OrgId {
        OrgId(uuid::Uuid::new_v4())
    }

    fn auth_sys_export(tenant_id: OrgId) -> NfsExportAuth {
        NfsExportAuth {
            path: "/data/project-a".into(),
            allowed_methods: vec![NfsAuthMethod::AuthSys],
            tenant_id,
            uid_mapping: UidMapping::AllToTenant,
        }
    }

    #[test]
    fn auth_sys_accepted_when_allowed() {
        let tenant = test_org_id();
        let export = auth_sys_export(tenant);
        let creds = NfsCredentials::from_auth_sys(1000, 1000, "client1.local".into());

        let result = validate_credentials(&creds, &export);
        assert_eq!(result.unwrap(), tenant);
    }

    #[test]
    fn method_not_allowed_rejected() {
        let tenant = test_org_id();
        let export = NfsExportAuth {
            path: "/data/secure".into(),
            allowed_methods: vec![NfsAuthMethod::Kerberos],
            tenant_id: tenant,
            uid_mapping: UidMapping::AllToTenant,
        };
        let creds = NfsCredentials::from_auth_sys(1000, 1000, "client1.local".into());

        let result = validate_credentials(&creds, &export);
        assert!(matches!(
            result,
            Err(NfsAuthError::MethodNotAllowed(NfsAuthMethod::AuthSys))
        ));
    }

    #[test]
    fn uid_range_mapping_works() {
        let tenant = test_org_id();
        let export = NfsExportAuth {
            path: "/data/multi".into(),
            allowed_methods: vec![NfsAuthMethod::AuthSys],
            tenant_id: tenant,
            uid_mapping: UidMapping::RangeMapping(vec![
                UidRange {
                    start: 1000,
                    end: 1999,
                    project_id: "proj-a".into(),
                },
                UidRange {
                    start: 2000,
                    end: 2999,
                    project_id: "proj-b".into(),
                },
            ]),
        };

        // UID in range — accepted.
        let creds_ok = NfsCredentials::from_auth_sys(1500, 1000, "client1.local".into());
        assert_eq!(validate_credentials(&creds_ok, &export).unwrap(), tenant);

        // UID out of range — rejected.
        let creds_bad = NfsCredentials::from_auth_sys(500, 1000, "client1.local".into());
        assert!(matches!(
            validate_credentials(&creds_bad, &export),
            Err(NfsAuthError::UidNotMapped(500))
        ));
    }

    #[test]
    fn kerberos_requires_principal() {
        let tenant = test_org_id();
        let export = NfsExportAuth {
            path: "/data/kerberized".into(),
            allowed_methods: vec![NfsAuthMethod::Kerberos],
            tenant_id: tenant,
            uid_mapping: UidMapping::AllToTenant,
        };

        // Missing principal — rejected.
        let creds_no_princ = NfsCredentials {
            method: NfsAuthMethod::Kerberos,
            uid: 1000,
            gid: 1000,
            hostname: "client1.local".into(),
            principal: None,
        };
        assert!(matches!(
            validate_credentials(&creds_no_princ, &export),
            Err(NfsAuthError::PrincipalNotFound(_))
        ));

        // With principal — accepted.
        let creds_ok = NfsCredentials {
            method: NfsAuthMethod::Kerberos,
            uid: 1000,
            gid: 1000,
            hostname: "client1.local".into(),
            principal: Some("user@REALM.COM".into()),
        };
        assert_eq!(validate_credentials(&creds_ok, &export).unwrap(), tenant);
    }

    #[test]
    fn from_auth_sys_constructor() {
        let creds = NfsCredentials::from_auth_sys(42, 100, "myhost".into());
        assert_eq!(creds.method, NfsAuthMethod::AuthSys);
        assert_eq!(creds.uid, 42);
        assert_eq!(creds.gid, 100);
        assert_eq!(creds.hostname, "myhost");
        assert!(creds.principal.is_none());
    }

    #[test]
    fn allows_method_check() {
        let export = NfsExportAuth {
            path: "/test".into(),
            allowed_methods: vec![NfsAuthMethod::AuthSys, NfsAuthMethod::Kerberos],
            tenant_id: test_org_id(),
            uid_mapping: UidMapping::AllToTenant,
        };

        assert!(export.allows_method(NfsAuthMethod::AuthSys));
        assert!(export.allows_method(NfsAuthMethod::Kerberos));
        assert!(!export.allows_method(NfsAuthMethod::None));
    }

    #[test]
    fn auth_none_rejected() {
        // NFS AUTH_NONE must be rejected when not in allowed methods.
        let tenant = test_org_id();
        let export = NfsExportAuth {
            path: "/data/secure".into(),
            allowed_methods: vec![NfsAuthMethod::AuthSys],
            tenant_id: tenant,
            uid_mapping: UidMapping::AllToTenant,
        };
        let creds = NfsCredentials {
            method: NfsAuthMethod::None,
            uid: 0,
            gid: 0,
            hostname: "anon-client".into(),
            principal: None,
        };
        let result = validate_credentials(&creds, &export);
        assert!(matches!(
            result,
            Err(NfsAuthError::MethodNotAllowed(NfsAuthMethod::None))
        ));
    }
}
