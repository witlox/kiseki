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
        if *self.read_only.read().unwrap() {
            return Err(ControlError::Rejected(
                "store is read-only: writes rejected (retriable)".into(),
            ));
        }

        let mut namespaces = self.namespaces.write().unwrap();
        if namespaces.contains_key(&ns.id) {
            return Err(ControlError::AlreadyExists(format!("namespace {}", ns.id)));
        }

        if ns.shard_id.is_empty() {
            let mut seq = self.shard_seq.write().unwrap();
            *seq += 1;
            ns.shard_id = format!("shard-{seq:04}");
        }

        namespaces.insert(ns.id.clone(), ns);
        Ok(())
    }

    /// Get a namespace by ID.
    pub fn get(&self, id: &str) -> Result<Namespace, ControlError> {
        let namespaces = self.namespaces.read().unwrap();
        namespaces
            .get(id)
            .cloned()
            .ok_or_else(|| ControlError::NotFound(format!("namespace {id}")))
    }

    /// List all namespaces.
    #[must_use]
    pub fn list(&self) -> Vec<Namespace> {
        let namespaces = self.namespaces.read().unwrap();
        namespaces.values().cloned().collect()
    }

    /// Set read-only mode on the store and all existing namespaces.
    pub fn set_read_only(&self, read_only: bool) {
        *self.read_only.write().unwrap() = read_only;
        let mut namespaces = self.namespaces.write().unwrap();
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
