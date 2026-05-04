//! Retention hold management.
//!
//! Retention holds prevent physical GC of chunks even when refcount
//! drops to zero. Used for litigation holds, compliance, etc.
//!
//! Spec: `ubiquitous-language.md#RetentionHold`, I-R1.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::ControlError;
use kiseki_common::locks::LockOrDie;

/// A retention hold on a namespace.
#[derive(Clone, Debug)]
pub struct Hold {
    /// Hold name.
    pub name: String,
    /// Target namespace.
    pub namespace_id: String,
    /// Whether the hold is active.
    pub active: bool,
}

/// In-memory retention hold store.
pub struct RetentionStore {
    holds: RwLock<HashMap<String, Hold>>,
}

impl RetentionStore {
    /// Create an empty retention store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            holds: RwLock::new(HashMap::new()),
        }
    }

    /// Create or activate a retention hold.
    pub fn set_hold(&self, name: &str, namespace_id: &str) -> Result<(), ControlError> {
        if name.is_empty() {
            return Err(ControlError::Rejected("hold name required".into()));
        }
        let mut holds = self.holds.write().lock_or_die("retention.unknown");
        holds.insert(
            name.to_owned(),
            Hold {
                name: name.to_owned(),
                namespace_id: namespace_id.to_owned(),
                active: true,
            },
        );
        Ok(())
    }

    /// Deactivate a retention hold.
    pub fn release_hold(&self, name: &str) -> Result<(), ControlError> {
        let mut holds = self.holds.write().lock_or_die("retention.unknown");
        let hold = holds
            .get_mut(name)
            .ok_or_else(|| ControlError::NotFound(format!("hold {name}")))?;
        hold.active = false;
        Ok(())
    }

    /// Check if any active hold exists for the given namespace.
    #[must_use]
    pub fn is_held(&self, namespace_id: &str) -> bool {
        let holds = self.holds.read().lock_or_die("retention.unknown");
        holds
            .values()
            .any(|h| h.namespace_id == namespace_id && h.active)
    }
}

impl Default for RetentionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_retention_hold_blocks_gc() {
        // Scenario: Set retention hold before crypto-shred
        let store = RetentionStore::new();
        store.set_hold("hipaa-litigation-2026", "trials").unwrap();

        // Hold is active — physical GC must be blocked
        assert!(
            store.is_held("trials"),
            "namespace under hold must report held"
        );

        // Verify the hold exists with correct metadata
        let holds = store.holds.read().lock_or_die("retention.unknown");
        let hold = holds.get("hipaa-litigation-2026").unwrap();
        assert!(hold.active);
        assert_eq!(hold.namespace_id, "trials");
        assert_eq!(hold.name, "hipaa-litigation-2026");
    }

    #[test]
    fn release_retention_hold_enables_gc() {
        // Scenario: Release retention hold
        let store = RetentionStore::new();
        store.set_hold("hipaa-litigation-2026", "trials").unwrap();
        assert!(store.is_held("trials"));

        // Release the hold
        store.release_hold("hipaa-litigation-2026").unwrap();

        // After release, chunks with refcount 0 become eligible for GC
        assert!(
            !store.is_held("trials"),
            "released hold should not block GC"
        );

        // Verify hold is inactive
        let holds = store.holds.read().lock_or_die("retention.unknown");
        let hold = holds.get("hipaa-litigation-2026").unwrap();
        assert!(!hold.active);
    }

    #[test]
    fn release_nonexistent_hold_fails() {
        let store = RetentionStore::new();
        assert!(store.release_hold("nonexistent").is_err());
    }
}
