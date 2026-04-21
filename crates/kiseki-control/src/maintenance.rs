//! Maintenance mode management.
//!
//! When enabled, all shards enter read-only mode and write commands
//! are rejected with retriable errors.
//!
//! Spec: `ubiquitous-language.md#MaintenanceMode`.

use std::sync::RwLock;

/// Cluster maintenance mode state.
pub struct MaintenanceState {
    enabled: RwLock<bool>,
}

impl MaintenanceState {
    /// Create a new state (disabled by default).
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: RwLock::new(false),
        }
    }

    /// Enable maintenance mode.
    pub fn enable(&self) {
        *self.enabled.write().unwrap() = true;
    }

    /// Disable maintenance mode.
    pub fn disable(&self) {
        *self.enabled.write().unwrap() = false;
    }

    /// Check if maintenance mode is active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read().unwrap()
    }
}

impl Default for MaintenanceState {
    fn default() -> Self {
        Self::new()
    }
}
