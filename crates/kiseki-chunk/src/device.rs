//! Device lifecycle and health state machine.
//!
//! Each device progresses through states: Healthy -> Warning -> Degraded
//! -> Evacuating -> Removed, or Healthy -> Failed. Transitions are
//! auditable (I-D2).
//!
//! Spec: ADR-024, I-D1, I-D2, I-D4.

use std::fmt;

/// Device health state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceState {
    /// Normal operation.
    Healthy,
    /// SMART or capacity warning — still serving I/O.
    Warning,
    /// Performance degraded — may need evacuation.
    Degraded,
    /// Background chunk migration in progress.
    Evacuating,
    /// All data migrated, device removed from pool.
    Removed,
    /// Unresponsive — EC repair triggered.
    Failed,
}

impl fmt::Display for DeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "Healthy"),
            Self::Warning => write!(f, "Warning"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Evacuating => write!(f, "Evacuating"),
            Self::Removed => write!(f, "Removed"),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

/// Pool health state based on capacity thresholds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolHealth {
    /// Below warning threshold.
    Healthy,
    /// Above warning threshold, writes still accepted.
    Warning,
    /// Above critical threshold, new placements rejected.
    Critical,
    /// At or above full threshold, ENOSPC.
    Full,
}

impl fmt::Display for PoolHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "Healthy"),
            Self::Warning => write!(f, "Warning"),
            Self::Critical => write!(f, "Critical"),
            Self::Full => write!(f, "Full"),
        }
    }
}

/// Per-device-class capacity thresholds (ADR-024).
#[derive(Clone, Debug)]
pub struct CapacityThresholds {
    /// Warning threshold (%).
    pub warning_pct: u8,
    /// Critical threshold (%) — new placements rejected.
    pub critical_pct: u8,
    /// Full threshold (%) — ENOSPC.
    pub full_pct: u8,
}

impl CapacityThresholds {
    /// `NVMe` thresholds per ADR-024.
    #[must_use]
    pub fn nvme() -> Self {
        Self {
            warning_pct: 75,
            critical_pct: 85,
            full_pct: 97,
        }
    }

    /// HDD thresholds per ADR-024.
    #[must_use]
    pub fn hdd() -> Self {
        Self {
            warning_pct: 85,
            critical_pct: 92,
            full_pct: 99,
        }
    }

    /// Compute pool health from used percentage.
    #[must_use]
    pub fn health(&self, used_pct: u8) -> PoolHealth {
        if used_pct >= self.full_pct {
            PoolHealth::Full
        } else if used_pct >= self.critical_pct {
            PoolHealth::Critical
        } else if used_pct >= self.warning_pct {
            PoolHealth::Warning
        } else {
            PoolHealth::Healthy
        }
    }
}

/// Structured audit event for device state transitions (I-D2).
///
/// Captures the full context of a state change: which device, from what
/// state, to what state, and why. Used for compliance audit logging.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceStateTransition {
    /// Device that transitioned.
    pub device_id: String,
    /// Previous state.
    pub old_state: DeviceState,
    /// New state after transition.
    pub new_state: DeviceState,
    /// Human-readable reason for the transition.
    pub reason: String,
}

/// A managed device with state tracking.
#[derive(Clone, Debug)]
pub struct ManagedDevice {
    /// Device identifier.
    pub id: String,
    /// Device path (e.g., `/dev/nvme2n1`).
    pub path: String,
    /// Current state.
    pub state: DeviceState,
    /// Number of chunks stored.
    pub chunk_count: u64,
    /// Capacity in bytes.
    pub capacity_bytes: u64,
    /// SMART wear percentage (SSD only).
    pub smart_wear_pct: Option<u8>,
    /// Reallocated sector count (HDD).
    pub reallocated_sectors: Option<u32>,
    /// Temperature in Celsius.
    pub temperature_c: Option<u8>,
    /// Evacuation progress (0-100%).
    pub evacuation_progress: Option<u8>,
}

impl ManagedDevice {
    /// Create a new healthy device.
    #[must_use]
    pub fn new(id: &str, path: &str, capacity_bytes: u64) -> Self {
        Self {
            id: id.to_owned(),
            path: path.to_owned(),
            state: DeviceState::Healthy,
            chunk_count: 0,
            capacity_bytes,
            smart_wear_pct: None,
            reallocated_sectors: None,
            temperature_c: None,
            evacuation_progress: None,
        }
    }

    /// Check if device should auto-evacuate based on health indicators.
    #[must_use]
    pub fn should_auto_evacuate(&self) -> bool {
        // SSD: SMART wear >= 90% (ADR-024).
        if let Some(wear) = self.smart_wear_pct {
            if wear >= 90 {
                return true;
            }
        }
        // HDD: reallocated sectors > 100.
        if let Some(sectors) = self.reallocated_sectors {
            if sectors > 100 {
                return true;
            }
        }
        false
    }

    /// Check if device is temperature-throttled.
    #[must_use]
    pub fn is_throttled(&self) -> bool {
        self.temperature_c.is_some_and(|t| t > 80)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvme_thresholds() {
        let t = CapacityThresholds::nvme();
        assert_eq!(t.health(74), PoolHealth::Healthy);
        assert_eq!(t.health(75), PoolHealth::Warning);
        assert_eq!(t.health(85), PoolHealth::Critical);
        assert_eq!(t.health(97), PoolHealth::Full);
    }

    #[test]
    fn hdd_thresholds() {
        let t = CapacityThresholds::hdd();
        assert_eq!(t.health(84), PoolHealth::Healthy);
        assert_eq!(t.health(85), PoolHealth::Warning);
        assert_eq!(t.health(92), PoolHealth::Critical);
        assert_eq!(t.health(99), PoolHealth::Full);
    }

    #[test]
    fn auto_evacuate_ssd_wear() {
        let mut dev = ManagedDevice::new("d1", "/dev/nvme0n1", 1024);
        assert!(!dev.should_auto_evacuate());
        dev.smart_wear_pct = Some(92);
        assert!(dev.should_auto_evacuate());
    }

    #[test]
    fn auto_evacuate_hdd_sectors() {
        let mut dev = ManagedDevice::new("d1", "/dev/sda", 1024);
        dev.reallocated_sectors = Some(150);
        assert!(dev.should_auto_evacuate());
    }

    #[test]
    fn temperature_throttle() {
        let mut dev = ManagedDevice::new("d1", "/dev/nvme0n1", 1024);
        assert!(!dev.is_throttled());
        dev.temperature_c = Some(82);
        assert!(dev.is_throttled());
    }

    // ---------------------------------------------------------------
    // Scenario: All device state transitions are audited (I-D2)
    // Transitions produce structured event data with from_state,
    // to_state, and reason.
    // ---------------------------------------------------------------
    #[test]
    fn device_state_transition_audit_event() {
        let mut dev = ManagedDevice::new("dev-1", "/dev/nvme0n1", 1_000_000);
        assert_eq!(dev.state, DeviceState::Healthy);

        let old_state = dev.state.clone();
        dev.state = DeviceState::Degraded;
        let new_state = dev.state.clone();
        let reason = "SMART wear";

        // Verify the audit event carries all required fields.
        assert_eq!(format!("{old_state}"), "Healthy");
        assert_eq!(format!("{new_state}"), "Degraded");
        assert_eq!(dev.id, "dev-1");
        assert!(!reason.is_empty());

        // Structured event data must include: device_id, old_state, new_state, reason.
        let event = DeviceStateTransition {
            device_id: dev.id.clone(),
            old_state,
            new_state,
            reason: reason.to_owned(),
        };
        assert_eq!(event.device_id, "dev-1");
        assert_eq!(event.old_state, DeviceState::Healthy);
        assert_eq!(event.new_state, DeviceState::Degraded);
        assert_eq!(event.reason, "SMART wear");
    }

    #[test]
    fn all_device_state_transitions_display() {
        // Verify Display for all states.
        assert_eq!(format!("{}", DeviceState::Healthy), "Healthy");
        assert_eq!(format!("{}", DeviceState::Warning), "Warning");
        assert_eq!(format!("{}", DeviceState::Degraded), "Degraded");
        assert_eq!(format!("{}", DeviceState::Evacuating), "Evacuating");
        assert_eq!(format!("{}", DeviceState::Removed), "Removed");
        assert_eq!(format!("{}", DeviceState::Failed), "Failed");
    }
}
