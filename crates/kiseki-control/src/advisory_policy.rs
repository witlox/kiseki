//! Advisory policy management.
//!
//! Profile allow-lists, hint budgets with inheritance, and opt-out
//! state machine. Federation replicates policy but NOT workflow state.
//!
//! Spec: I-WA7, I-WA18, ADR-021 section 6.

use crate::error::ControlError;

/// Per-workload advisory rate limits.
#[derive(Clone, Debug, Default)]
pub struct HintBudget {
    /// Hints per second ceiling.
    pub hints_per_sec: u32,
    /// Maximum concurrent workflows.
    pub max_concurrent_flows: u32,
    /// Phases per workflow.
    pub phases_per_workflow: u32,
    /// Prefetch bytes ceiling.
    pub prefetch_bytes_max: u64,
}

/// Which workload profiles are allowed at a scope.
#[derive(Clone, Debug, Default)]
pub struct ProfilePolicy {
    /// Allowed profile names.
    pub allowed_profiles: Vec<String>,
}

/// Advisory opt-out FSM state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptOutState {
    /// Advisory is active.
    Enabled,
    /// Advisory is being shut down gracefully.
    Draining,
    /// Advisory is fully disabled.
    Disabled,
}

/// Advisory policy at a specific scope (org/project/workload).
#[derive(Clone, Debug)]
pub struct ScopePolicy {
    /// Scope identifier.
    pub scope_id: String,
    /// Parent scope identifier (empty for org-level).
    pub parent_id: String,
    /// Rate limit budget.
    pub budget: HintBudget,
    /// Profile allow-list.
    pub profiles: ProfilePolicy,
    /// Opt-out state.
    pub opt_out: OptOutState,
}

/// Validate that a child budget does not exceed the parent ceiling.
pub fn validate_budget_inheritance(
    parent: &HintBudget,
    child: &HintBudget,
) -> Result<(), ControlError> {
    if child.hints_per_sec > parent.hints_per_sec {
        return Err(ControlError::QuotaExceeded(format!(
            "hints/sec {} exceeds parent ceiling {}",
            child.hints_per_sec, parent.hints_per_sec
        )));
    }
    if child.max_concurrent_flows > parent.max_concurrent_flows {
        return Err(ControlError::QuotaExceeded(format!(
            "concurrent flows {} exceeds parent ceiling {}",
            child.max_concurrent_flows, parent.max_concurrent_flows
        )));
    }
    if child.prefetch_bytes_max > parent.prefetch_bytes_max {
        return Err(ControlError::QuotaExceeded(format!(
            "prefetch bytes {} exceeds parent ceiling {}",
            child.prefetch_bytes_max, parent.prefetch_bytes_max
        )));
    }
    Ok(())
}

/// Validate that a child's profiles are a subset of the parent's.
pub fn validate_profile_inheritance(
    parent: &ProfilePolicy,
    child: &ProfilePolicy,
) -> Result<(), ControlError> {
    for profile in &child.allowed_profiles {
        if !parent.allowed_profiles.contains(profile) {
            return Err(ControlError::Rejected(format!(
                "profile {profile:?} not in parent allow-list"
            )));
        }
    }
    Ok(())
}
