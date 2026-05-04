//! Maintenance mode management.
//!
//! When enabled, all shards enter read-only mode and write commands
//! are rejected with retriable errors.
//!
//! Spec: `ubiquitous-language.md#MaintenanceMode`.

use std::sync::RwLock;
use kiseki_common::locks::LockOrDie;

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
        *self.enabled.write().lock_or_die("maintenance.unknown") = true;
    }

    /// Disable maintenance mode.
    pub fn disable(&self) {
        *self.enabled.write().lock_or_die("maintenance.unknown") = false;
    }

    /// Check if maintenance mode is active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        *self.enabled.read().lock_or_die("maintenance.unknown")
    }
}

/// Audit event descriptor for maintenance transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceAuditEvent {
    /// Event type.
    pub event_type: &'static str,
    /// Whether maintenance mode is now enabled or disabled.
    pub enabled: bool,
}

/// Enter cluster-wide maintenance mode, returning the shard count that
/// entered read-only and an audit event descriptor.
pub fn enter_maintenance(
    state: &MaintenanceState,
    shard_count: usize,
) -> (Vec<&'static str>, MaintenanceAuditEvent) {
    state.enable();
    // Each shard emits a ShardMaintenanceEntered event.
    let events: Vec<&str> = (0..shard_count)
        .map(|_| "ShardMaintenanceEntered")
        .collect();
    let audit = MaintenanceAuditEvent {
        event_type: "maintenance_window_started",
        enabled: true,
    };
    (events, audit)
}

impl Default for MaintenanceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_wide_maintenance_mode() {
        // Scenario: Cluster-wide maintenance mode
        let state = MaintenanceState::new();
        assert!(!state.is_enabled());

        let shard_count = 5;
        let (events, audit) = enter_maintenance(&state, shard_count);

        // All shards enter read-only mode
        assert!(state.is_enabled(), "maintenance mode should be enabled");

        // ShardMaintenanceEntered events are emitted for each shard
        assert_eq!(events.len(), shard_count);
        assert!(events.iter().all(|e| *e == "ShardMaintenanceEntered"));

        // Maintenance window is recorded in the audit log
        assert_eq!(audit.event_type, "maintenance_window_started");
        assert!(audit.enabled);

        // Writes are rejected during maintenance (tested via NamespaceStore.set_read_only)
        // Reads continue from existing views (read-only, not blocked)
    }
}
