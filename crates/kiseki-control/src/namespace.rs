//! Namespace management.
//!
//! A namespace is a logical storage container within a tenant hierarchy.
//! Each namespace maps to one or more shards. Compliance tags inherit
//! downward from the org/project.
//!
//! Spec: `ubiquitous-language.md#Namespace`, I-T1.

use std::collections::HashMap;
use std::sync::RwLock;

use kiseki_common::tenancy::ComplianceTag;

use crate::error::ControlError;
use kiseki_common::locks::LockOrDie;

/// Namespace within a tenant hierarchy.
#[derive(Clone, Debug)]
pub struct Namespace {
    /// Unique identifier.
    pub id: String,
    /// Parent organization ID.
    pub org_id: String,
    /// Parent project ID (may be empty).
    pub project_id: String,
    /// Assigned shard ID (auto-generated if empty).
    pub shard_id: String,
    /// Compliance tags (inherited + own).
    pub compliance_tags: Vec<ComplianceTag>,
    /// Read-only flag (set during maintenance).
    pub read_only: bool,
}

/// In-memory namespace store.
pub struct NamespaceStore {
    namespaces: RwLock<HashMap<String, Namespace>>,
    shard_seq: RwLock<u32>,
    read_only: RwLock<bool>,
}

impl NamespaceStore {
    /// Create an empty namespace store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            namespaces: RwLock::new(HashMap::new()),
            shard_seq: RwLock::new(0),
            read_only: RwLock::new(false),
        }
    }

    /// Create a new namespace, auto-assigning a shard if none provided.
    pub fn create(&self, mut ns: Namespace) -> Result<(), ControlError> {
        if *self.read_only.read().lock_or_die("namespace.unknown") {
            return Err(ControlError::Rejected(
                "store is read-only: writes rejected (retriable)".into(),
            ));
        }

        let mut namespaces = self.namespaces.write().lock_or_die("namespace.unknown");
        if namespaces.contains_key(&ns.id) {
            return Err(ControlError::AlreadyExists(format!("namespace {}", ns.id)));
        }

        if ns.shard_id.is_empty() {
            let mut seq = self.shard_seq.write().lock_or_die("namespace.unknown");
            *seq += 1;
            ns.shard_id = format!("shard-{seq:04}");
        }

        namespaces.insert(ns.id.clone(), ns);
        Ok(())
    }

    /// Get a namespace by ID.
    pub fn get(&self, id: &str) -> Result<Namespace, ControlError> {
        let namespaces = self.namespaces.read().lock_or_die("namespace.unknown");
        namespaces
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound(format!("namespace {id}")))
    }

    /// List all namespaces.
    #[must_use]
    pub fn list(&self) -> Vec<Namespace> {
        let namespaces = self.namespaces.read().lock_or_die("namespace.unknown");
        namespaces.values().cloned().collect()
    }

    /// Check whether a compliance tag can be removed from a namespace.
    ///
    /// A tag cannot be removed if the namespace contains compositions
    /// (i.e. has data). The caller must provide the composition count.
    pub fn can_remove_compliance_tag(
        &self,
        namespace_id: &str,
        _tag: &kiseki_common::tenancy::ComplianceTag,
        composition_count: u64,
    ) -> Result<(), ControlError> {
        // Verify namespace exists.
        let namespaces = self.namespaces.read().lock_or_die("namespace.unknown");
        if !namespaces.contains_key(namespace_id) {
            return Err(ControlError::NotFound(format!("namespace {namespace_id}")));
        }
        if composition_count > 0 {
            return Err(ControlError::Rejected(
                "cannot remove compliance tag with existing data; migrate or delete first".into(),
            ));
        }
        Ok(())
    }

    /// Set read-only mode on the store and all existing namespaces.
    pub fn set_read_only(&self, read_only: bool) {
        *self.read_only.write().lock_or_die("namespace.unknown") = read_only;
        let mut namespaces = self.namespaces.write().lock_or_die("namespace.unknown");
        for ns in namespaces.values_mut() {
            ns.read_only = read_only;
        }
    }
}

impl Default for NamespaceStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::tenancy::ComplianceTag;

    #[test]
    fn compliance_tag_removal_blocked_with_existing_data() {
        // Scenario: Compliance tag cannot be removed if data exists under it
        let store = NamespaceStore::new();
        let ns = Namespace {
            id: "trials".into(),
            org_id: "org-pharma".into(),
            project_id: String::new(),
            shard_id: String::new(),
            compliance_tags: vec![ComplianceTag::Hipaa],
            read_only: false,
        };
        store.create(ns).unwrap();

        // Namespace has compositions (data exists) -> removal rejected
        let result = store.can_remove_compliance_tag("trials", &ComplianceTag::Hipaa, 5);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cannot remove compliance tag"), "error: {msg}");
        assert!(
            msg.contains("migrate or delete"),
            "error should suggest migration: {msg}"
        );

        // No compositions -> removal allowed
        assert!(store
            .can_remove_compliance_tag("trials", &ComplianceTag::Hipaa, 0)
            .is_ok());
    }
}
