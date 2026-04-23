//! Workflow table — tracks active workflows per workload.

use std::collections::HashMap;

use kiseki_common::advisory::{PhaseId, WorkflowRef, WorkloadProfile};

use crate::error::AdvisoryError;

/// A single active workflow entry.
#[derive(Clone, Debug)]
pub struct WorkflowEntry {
    /// Opaque workflow reference.
    pub workflow_ref: WorkflowRef,
    /// Workload profile declared at creation.
    pub profile: WorkloadProfile,
    /// Current phase (monotonically increasing, I-WA13).
    pub current_phase: PhaseId,
    /// Phase history (last K phases).
    pub phase_history: Vec<PhaseId>,
    /// Max phases to retain in history.
    max_history: usize,
}

impl WorkflowEntry {
    /// Create a new workflow entry.
    #[must_use]
    pub fn new(
        workflow_ref: WorkflowRef,
        profile: WorkloadProfile,
        initial_phase: PhaseId,
        max_history: usize,
    ) -> Self {
        Self {
            workflow_ref,
            profile,
            current_phase: initial_phase,
            phase_history: vec![initial_phase],
            max_history,
        }
    }

    /// Advance to a new phase. Must be strictly greater than current (I-WA13).
    pub fn advance_phase(&mut self, new_phase: PhaseId) -> Result<(), AdvisoryError> {
        if new_phase <= self.current_phase {
            return Err(AdvisoryError::PhaseNotMonotonic {
                current: self.current_phase.0,
                requested: new_phase.0,
            });
        }
        self.current_phase = new_phase;
        self.phase_history.push(new_phase);
        if self.phase_history.len() > self.max_history {
            self.phase_history.remove(0);
        }
        Ok(())
    }
}

/// Table of active workflows indexed by `WorkflowRef`.
pub struct WorkflowTable {
    entries: HashMap<WorkflowRef, WorkflowEntry>,
}

impl WorkflowTable {
    /// Create an empty workflow table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Declare a new workflow.
    pub fn declare(
        &mut self,
        workflow_ref: WorkflowRef,
        profile: WorkloadProfile,
        initial_phase: PhaseId,
    ) -> &WorkflowEntry {
        let entry = WorkflowEntry::new(workflow_ref, profile, initial_phase, 10);
        self.entries.insert(workflow_ref, entry);
        self.entries
            .get(&workflow_ref)
            .unwrap_or_else(|| unreachable!())
    }

    /// Get a workflow by ref.
    #[must_use]
    pub fn get(&self, workflow_ref: &WorkflowRef) -> Option<&WorkflowEntry> {
        self.entries.get(workflow_ref)
    }

    /// Get a mutable workflow by ref.
    pub fn get_mut(&mut self, workflow_ref: &WorkflowRef) -> Option<&mut WorkflowEntry> {
        self.entries.get_mut(workflow_ref)
    }

    /// End a workflow (remove from table).
    pub fn end(&mut self, workflow_ref: &WorkflowRef) -> bool {
        self.entries.remove(workflow_ref).is_some()
    }

    /// Number of active workflows.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.entries.len()
    }
}

impl Default for WorkflowTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ref() -> WorkflowRef {
        WorkflowRef([0x01; 16])
    }

    #[test]
    fn declare_and_get() {
        let mut table = WorkflowTable::new();
        table.declare(test_ref(), WorkloadProfile::AiTraining, PhaseId(1));

        let entry = table.get(&test_ref());
        assert!(entry.is_some());
        assert_eq!(
            entry.unwrap_or_else(|| unreachable!()).current_phase,
            PhaseId(1)
        );
    }

    #[test]
    fn phase_monotonicity() {
        let mut table = WorkflowTable::new();
        table.declare(test_ref(), WorkloadProfile::AiTraining, PhaseId(1));

        let entry = table.get_mut(&test_ref()).unwrap_or_else(|| unreachable!());

        // Forward advance succeeds.
        assert!(entry.advance_phase(PhaseId(2)).is_ok());
        assert!(entry.advance_phase(PhaseId(5)).is_ok());

        // Backward fails (I-WA13).
        assert!(entry.advance_phase(PhaseId(3)).is_err());

        // Same value fails.
        assert!(entry.advance_phase(PhaseId(5)).is_err());
    }

    #[test]
    fn end_workflow() {
        let mut table = WorkflowTable::new();
        table.declare(test_ref(), WorkloadProfile::AiTraining, PhaseId(1));
        assert_eq!(table.active_count(), 1);

        assert!(table.end(&test_ref()));
        assert_eq!(table.active_count(), 0);
        assert!(table.get(&test_ref()).is_none());
    }

    #[test]
    fn phase_monotonicity_forward_then_backward_rejected() {
        let mut entry = WorkflowEntry::new(test_ref(), WorkloadProfile::AiTraining, PhaseId(1), 10);

        // Advance 1 → 2 → 3 succeeds.
        assert!(entry.advance_phase(PhaseId(2)).is_ok());
        assert!(entry.advance_phase(PhaseId(3)).is_ok());
        assert_eq!(entry.current_phase, PhaseId(3));

        // Going backward to phase 1 is rejected (I-WA13).
        let err = entry.advance_phase(PhaseId(1));
        assert!(err.is_err());
        assert!(matches!(
            err.unwrap_err(),
            AdvisoryError::PhaseNotMonotonic {
                current: 3,
                requested: 1
            }
        ));
    }

    #[test]
    fn phase_history_tracks_advances() {
        let mut entry = WorkflowEntry::new(test_ref(), WorkloadProfile::AiTraining, PhaseId(1), 10);

        entry
            .advance_phase(PhaseId(2))
            .unwrap_or_else(|_| unreachable!());
        entry
            .advance_phase(PhaseId(3))
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(
            entry.phase_history,
            vec![PhaseId(1), PhaseId(2), PhaseId(3)]
        );
    }
}
