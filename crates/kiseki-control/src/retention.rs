//! Retention hold management.
//!
//! Retention holds prevent physical GC of chunks even when refcount
//! drops to zero. Used for litigation holds, compliance, etc.
//!
//! Spec: `ubiquitous-language.md#RetentionHold`, I-R1.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::ControlError;

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
        let mut holds = self.holds.write().unwrap();
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
        let mut holds = self.holds.write().unwrap();
        let hold = holds
            .get_mut(name)
            .ok_or_else(|| ControlError::NotFound(format!("hold {name}")))?;
        hold.active = false;
        Ok(())
    }

    /// Check if any active hold exists for the given namespace.
    #[must_use]
    pub fn is_held(&self, namespace_id: &str) -> bool {
        let holds = self.holds.read().unwrap();
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
