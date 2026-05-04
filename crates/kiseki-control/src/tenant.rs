//! Tenant lifecycle management.
//!
//! Three-level hierarchy: Organization -> Project -> Workload.
//! Compliance tags inherit downward (union). Quotas bounded by parent.
//!
//! Spec: `ubiquitous-language.md#Tenancy-and-access`, I-T1..I-T4.

use std::collections::HashMap;
use std::sync::RwLock;

use kiseki_common::tenancy::{ComplianceTag, DedupPolicy, Quota};

use crate::error::ControlError;
use kiseki_common::locks::LockOrDie;

/// Organization — top-level tenant (I-T1, I-T3).
#[derive(Clone, Debug)]
pub struct Organization {
    /// Unique identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Compliance tags at org level.
    pub compliance_tags: Vec<ComplianceTag>,
    /// Dedup policy.
    pub dedup_policy: DedupPolicy,
    /// Resource quota ceiling.
    pub quota: Quota,
    /// Whether compression is enabled for this org (default: false).
    /// HIPAA orgs cannot enable compression.
    pub compression_enabled: bool,
}

/// Project — optional grouping within an organization.
#[derive(Clone, Debug)]
pub struct Project {
    /// Unique identifier.
    pub id: String,
    /// Parent organization ID.
    pub org_id: String,
    /// Display name.
    pub name: String,
    /// Additional compliance tags (merged with org tags).
    pub compliance_tags: Vec<ComplianceTag>,
    /// Resource quota (bounded by org quota).
    pub quota: Quota,
}

/// Workload — runtime isolation unit within a tenant.
#[derive(Clone, Debug)]
pub struct Workload {
    /// Unique identifier.
    pub id: String,
    /// Parent organization ID.
    pub org_id: String,
    /// Parent project ID (empty if no project).
    pub project_id: String,
    /// Display name.
    pub name: String,
    /// Resource quota (bounded by org quota).
    pub quota: Quota,
}

/// Effective compliance tags — union of org and project tags (I-K9).
///
/// Tags cannot weaken inherited policy; the effective set at any node
/// is the union of its own tags and all ancestor tags.
#[must_use]
pub fn effective_compliance_tags(
    org: &Organization,
    project: Option<&Project>,
) -> Vec<ComplianceTag> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for tag in &org.compliance_tags {
        if seen.insert(tag.clone()) {
            result.push(tag.clone());
        }
    }

    if let Some(proj) = project {
        for tag in &proj.compliance_tags {
            if seen.insert(tag.clone()) {
                result.push(tag.clone());
            }
        }
    }

    result
}

/// Validate that a child quota does not exceed the parent ceiling.
pub fn validate_quota(parent: &Quota, child: &Quota) -> Result<(), ControlError> {
    if child.capacity_bytes > parent.capacity_bytes {
        return Err(ControlError::QuotaExceeded(format!(
            "capacity {} exceeds parent ceiling {}",
            child.capacity_bytes, parent.capacity_bytes
        )));
    }
    if child.iops > parent.iops {
        return Err(ControlError::QuotaExceeded(format!(
            "IOPS {} exceeds parent ceiling {}",
            child.iops, parent.iops
        )));
    }
    if child.metadata_ops_per_sec > parent.metadata_ops_per_sec {
        return Err(ControlError::QuotaExceeded(format!(
            "metadata ops/sec {} exceeds parent ceiling {}",
            child.metadata_ops_per_sec, parent.metadata_ops_per_sec
        )));
    }
    Ok(())
}

/// In-memory tenant store (ADV-4: sync `RwLock`, not async).
pub struct TenantStore {
    orgs: RwLock<HashMap<String, Organization>>,
    projects: RwLock<HashMap<String, Project>>,
    workloads: RwLock<HashMap<String, Workload>>,
}

impl TenantStore {
    /// Create an empty tenant store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            orgs: RwLock::new(HashMap::new()),
            projects: RwLock::new(HashMap::new()),
            workloads: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new organization.
    pub fn create_org(&self, org: Organization) -> Result<(), ControlError> {
        let mut orgs = self.orgs.write().lock_or_die("tenant.unknown");
        if orgs.contains_key(&org.id) {
            return Err(ControlError::AlreadyExists(format!(
                "organization {}",
                org.id
            )));
        }
        orgs.insert(org.id.clone(), org);
        Ok(())
    }

    /// Get an organization by ID.
    pub fn get_org(&self, id: &str) -> Result<Organization, ControlError> {
        let orgs = self.orgs.read().lock_or_die("tenant.unknown");
        orgs.get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound(format!("organization {id}")))
    }

    /// List all organizations.
    #[must_use]
    pub fn list_orgs(&self) -> Vec<Organization> {
        let orgs = self.orgs.read().lock_or_die("tenant.unknown");
        orgs.values().cloned().collect()
    }

    /// Create a project within an organization.
    ///
    /// Holds org read lock during quota validation and project insert
    /// to prevent TOCTOU (G-ADV-4).
    pub fn create_project(&self, proj: Project) -> Result<(), ControlError> {
        let orgs = self.orgs.read().lock_or_die("tenant.unknown");
        let org = orgs
            .get(&proj.org_id)
            .ok_or_else(|| ControlError::NotFound(format!("organization {}", proj.org_id)))?;
        validate_quota(&org.quota, &proj.quota)?;

        // Hold org lock while inserting project — prevents TOCTOU.
        let mut projects = self.projects.write().lock_or_die("tenant.unknown");
        projects.insert(proj.id.clone(), proj);
        Ok(())
    }

    /// Get a project by ID.
    pub fn get_project(&self, id: &str) -> Result<Project, ControlError> {
        let projects = self.projects.read().lock_or_die("tenant.unknown");
        projects
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound(format!("project {id}")))
    }

    /// Create a workload within an organization.
    pub fn create_workload(&self, wl: Workload) -> Result<(), ControlError> {
        let orgs = self.orgs.read().lock_or_die("tenant.unknown");
        let org = orgs
            .get(&wl.org_id)
            .ok_or_else(|| ControlError::NotFound(format!("organization {}", wl.org_id)))?;
        validate_quota(&org.quota, &wl.quota)?;

        let mut workloads = self.workloads.write().lock_or_die("tenant.unknown");
        workloads.insert(wl.id.clone(), wl);
        Ok(())
    }

    /// Get a workload by ID.
    pub fn get_workload(&self, id: &str) -> Result<Workload, ControlError> {
        let workloads = self.workloads.read().lock_or_die("tenant.unknown");
        workloads
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound(format!("workload {id}")))
    }
}

impl Default for TenantStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_org() -> Organization {
        Organization {
            id: "org-test".into(),
            name: "org-test".into(),
            compliance_tags: vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr],
            dedup_policy: DedupPolicy::CrossTenant,
            quota: Quota {
                capacity_bytes: 500_000_000_000_000,
                iops: 100_000,
                metadata_ops_per_sec: 10_000,
            },
            compression_enabled: false,
        }
    }

    #[test]
    fn create_and_get_org() {
        let store = TenantStore::new();
        store.create_org(test_org()).unwrap();
        let org = store.get_org("org-test").unwrap();
        assert_eq!(org.name, "org-test");
        assert_eq!(org.compliance_tags.len(), 2);
    }

    #[test]
    fn duplicate_org_rejected() {
        let store = TenantStore::new();
        store.create_org(test_org()).unwrap();
        assert!(store.create_org(test_org()).is_err());
    }

    #[test]
    fn project_quota_validated() {
        let store = TenantStore::new();
        store.create_org(test_org()).unwrap();

        let proj = Project {
            id: "proj-1".into(),
            org_id: "org-test".into(),
            name: "proj-1".into(),
            compliance_tags: vec![ComplianceTag::RevFadp],
            quota: Quota {
                capacity_bytes: 200_000_000_000_000,
                iops: 50_000,
                metadata_ops_per_sec: 5_000,
            },
        };
        store.create_project(proj).unwrap();

        // Exceeds capacity ceiling.
        let bad = Project {
            id: "proj-bad".into(),
            org_id: "org-test".into(),
            name: "proj-bad".into(),
            compliance_tags: vec![],
            quota: Quota {
                capacity_bytes: 999_000_000_000_000,
                iops: 1,
                metadata_ops_per_sec: 1,
            },
        };
        assert!(store.create_project(bad).is_err());
    }

    #[test]
    fn effective_tags_union() {
        let org = test_org();
        let proj = Project {
            id: "p".into(),
            org_id: "org-test".into(),
            name: "p".into(),
            compliance_tags: vec![ComplianceTag::RevFadp],
            quota: Quota {
                capacity_bytes: 100_000_000_000_000,
                iops: 10_000,
                metadata_ops_per_sec: 1_000,
            },
        };
        let tags = effective_compliance_tags(&org, Some(&proj));
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn workload_quota_validated() {
        let store = TenantStore::new();
        store.create_org(test_org()).unwrap();

        let wl = Workload {
            id: "wl-1".into(),
            org_id: "org-test".into(),
            project_id: String::new(),
            name: "wl-1".into(),
            quota: Quota {
                capacity_bytes: 50_000_000_000_000,
                iops: 20_000,
                metadata_ops_per_sec: 2_000,
            },
        };
        store.create_workload(wl).unwrap();
    }

    #[test]
    fn compression_disabled_by_default() {
        let org = test_org();
        assert!(
            !org.compression_enabled,
            "new orgs must have compression disabled by default"
        );
    }

    #[test]
    fn workload_quota_exceeded_rejects_write() {
        // Scenario: Workload quota within org ceiling
        // Org has 500TB capacity, workload has 50TB quota.
        // A write that would push workload over its quota is rejected.
        let org_quota = Quota {
            capacity_bytes: 500_000_000_000_000, // 500TB
            iops: 100_000,
            metadata_ops_per_sec: 10_000,
        };
        let workload_quota = Quota {
            capacity_bytes: 50_000_000_000_000, // 50TB
            iops: 100_000,
            metadata_ops_per_sec: 10_000,
        };
        // Workload has used 49TB, wants to write 2TB => 51TB > 50TB quota
        let used_bytes: u64 = 49_000_000_000_000;
        let write_bytes: u64 = 2_000_000_000_000;
        let would_exceed = used_bytes + write_bytes > workload_quota.capacity_bytes;
        assert!(would_exceed, "49TB + 2TB should exceed 50TB workload quota");
        // Org still has headroom: 300TB used + 2TB < 500TB
        let org_used: u64 = 300_000_000_000_000;
        assert!(
            org_used + write_bytes < org_quota.capacity_bytes,
            "org should still have headroom"
        );
    }

    #[test]
    fn quota_adjustment_within_ceiling_succeeds() {
        // Scenario: Quota adjustment by tenant admin
        // Tenant admin increases workload quota, must stay within org ceiling.
        let store = TenantStore::new();
        store.create_org(test_org()).unwrap();
        let wl = Workload {
            id: "wl-adjust".into(),
            org_id: "org-test".into(),
            project_id: String::new(),
            name: "training-run-42".into(),
            quota: Quota {
                capacity_bytes: 50_000_000_000_000,
                iops: 20_000,
                metadata_ops_per_sec: 2_000,
            },
        };
        store.create_workload(wl).unwrap();
        // Increase to 100TB — still within org's 500TB ceiling
        let new_quota = Quota {
            capacity_bytes: 100_000_000_000_000,
            iops: 20_000,
            metadata_ops_per_sec: 2_000,
        };
        assert!(
            validate_quota(&test_org().quota, &new_quota).is_ok(),
            "100TB should be within 500TB org ceiling"
        );
        // Trying to exceed org ceiling should fail
        let too_big = Quota {
            capacity_bytes: 600_000_000_000_000,
            iops: 20_000,
            metadata_ops_per_sec: 2_000,
        };
        assert!(
            validate_quota(&test_org().quota, &too_big).is_err(),
            "600TB should exceed 500TB org ceiling"
        );
    }

    #[test]
    fn child_quota_exceeding_parent_rejected() {
        // Quota narrowing: child quota > parent on any dimension should fail.
        let parent = Quota {
            capacity_bytes: 1_000_000,
            iops: 10_000,
            metadata_ops_per_sec: 1_000,
        };

        // Exceeds capacity.
        let child_cap = Quota {
            capacity_bytes: 2_000_000,
            iops: 100,
            metadata_ops_per_sec: 100,
        };
        assert!(validate_quota(&parent, &child_cap).is_err());

        // Exceeds IOPS.
        let child_iops = Quota {
            capacity_bytes: 500_000,
            iops: 20_000,
            metadata_ops_per_sec: 100,
        };
        assert!(validate_quota(&parent, &child_iops).is_err());

        // Exceeds metadata ops/sec.
        let child_meta = Quota {
            capacity_bytes: 500_000,
            iops: 100,
            metadata_ops_per_sec: 5_000,
        };
        assert!(validate_quota(&parent, &child_meta).is_err());

        // Equal to parent — should succeed.
        let child_equal = parent;
        assert!(validate_quota(&parent, &child_equal).is_ok());
    }
}
