//! Token-bucket budget enforcement per workload.
//!
//! Rate-limits advisory operations (hints/sec, declare/sec) to prevent
//! a single workload from overwhelming the advisory subsystem.

use crate::error::AdvisoryError;

/// Budget configuration for a workload.
#[derive(Clone, Debug)]
pub struct BudgetConfig {
    /// Maximum hints per second.
    pub hints_per_sec: u32,
    /// Maximum concurrent workflows.
    pub max_concurrent_workflows: u32,
    /// Maximum phases per workflow.
    pub max_phases_per_workflow: u32,
}

/// Token-bucket enforcer for a single workload.
pub struct BudgetEnforcer {
    config: BudgetConfig,
    /// Hints consumed in the current window.
    hints_this_window: u32,
    /// Active workflow count.
    active_workflows: u32,
}

impl BudgetEnforcer {
    /// Create a new enforcer with the given budget.
    #[must_use]
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            hints_this_window: 0,
            active_workflows: 0,
        }
    }

    /// Try to consume a hint token. Returns `Err` if budget exceeded.
    pub fn try_hint(&mut self) -> Result<(), AdvisoryError> {
        if self.hints_this_window >= self.config.hints_per_sec {
            return Err(AdvisoryError::BudgetExceeded("hints/sec".into()));
        }
        self.hints_this_window += 1;
        Ok(())
    }

    /// Try to declare a new workflow. Returns `Err` if at capacity.
    pub fn try_declare(&mut self) -> Result<(), AdvisoryError> {
        if self.active_workflows >= self.config.max_concurrent_workflows {
            return Err(AdvisoryError::BudgetExceeded("concurrent workflows".into()));
        }
        self.active_workflows += 1;
        Ok(())
    }

    /// Release a workflow slot (workflow ended or expired).
    pub fn release_workflow(&mut self) {
        self.active_workflows = self.active_workflows.saturating_sub(1);
    }

    /// Reset the per-second hint counter (called by a 1-second ticker).
    pub fn reset_window(&mut self) {
        self.hints_this_window = 0;
    }

    /// Current hint usage in this window.
    #[must_use]
    pub fn hints_used(&self) -> u32 {
        self.hints_this_window
    }

    /// Current active workflow count.
    #[must_use]
    pub fn active_workflows(&self) -> u32 {
        self.active_workflows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> BudgetConfig {
        BudgetConfig {
            hints_per_sec: 3,
            max_concurrent_workflows: 2,
            max_phases_per_workflow: 10,
        }
    }

    #[test]
    fn hint_budget_enforcement() {
        let mut enforcer = BudgetEnforcer::new(test_config());

        assert!(enforcer.try_hint().is_ok());
        assert!(enforcer.try_hint().is_ok());
        assert!(enforcer.try_hint().is_ok());
        // 4th hint exceeds budget.
        assert!(enforcer.try_hint().is_err());

        // Reset allows more.
        enforcer.reset_window();
        assert!(enforcer.try_hint().is_ok());
    }

    #[test]
    fn workflow_budget_enforcement() {
        let mut enforcer = BudgetEnforcer::new(test_config());

        assert!(enforcer.try_declare().is_ok());
        assert!(enforcer.try_declare().is_ok());
        // 3rd exceeds budget.
        assert!(enforcer.try_declare().is_err());

        // Release one, then can declare again.
        enforcer.release_workflow();
        assert!(enforcer.try_declare().is_ok());
    }
}
