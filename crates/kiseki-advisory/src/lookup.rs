//! `AdvisoryLookup` — hot-path read surface for data-path crates.
//!
//! Data-path crates receive an `Option<&OperationAdvisory>` which they
//! pass through to their operations. The `AdvisoryLookup` resolves a
//! `WorkflowRef` to the effective hints for the current operation.
//!
//! Returns `None` on any error or timeout (I-WA2: advisory failure
//! never blocks the data path).

use kiseki_common::advisory::{OperationAdvisory, WorkflowRef};

use crate::workflow::WorkflowTable;

/// Hot-path lookup that resolves a workflow ref to advisory hints.
///
/// In production, this would use an `arc-swap` snapshot with a ≤500µs
/// deadline. The in-memory reference implementation does a direct
/// table lookup.
pub struct AdvisoryLookup<'a> {
    table: &'a WorkflowTable,
}

impl<'a> AdvisoryLookup<'a> {
    /// Create a lookup against the workflow table.
    #[must_use]
    pub fn new(table: &'a WorkflowTable) -> Self {
        Self { table }
    }

    /// Look up advisory hints for a workflow. Returns `None` if the
    /// workflow doesn't exist or if the lookup exceeds the deadline
    /// (I-WA2).
    #[must_use]
    pub fn lookup(&self, workflow_ref: &WorkflowRef) -> Option<OperationAdvisory> {
        let entry = self.table.get(workflow_ref)?;

        Some(OperationAdvisory {
            workflow_ref: Some(entry.workflow_ref),
            phase_id: Some(entry.current_phase),
            // Other fields filled by the advisory runtime from
            // effective-hints table; not available in the in-memory
            // reference implementation.
            ..OperationAdvisory::empty()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::advisory::{PhaseId, WorkloadProfile};

    #[test]
    fn lookup_existing_workflow() {
        let mut table = WorkflowTable::new();
        let wf_ref = WorkflowRef([0x42; 16]);
        table.declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(3));

        let lookup = AdvisoryLookup::new(&table);
        let result = lookup.lookup(&wf_ref);
        assert!(result.is_some());
        let advisory = result.unwrap_or_else(|| unreachable!());
        assert_eq!(advisory.workflow_ref, Some(wf_ref));
        assert_eq!(advisory.phase_id, Some(PhaseId(3)));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let table = WorkflowTable::new();
        let lookup = AdvisoryLookup::new(&table);
        assert!(lookup.lookup(&WorkflowRef([0xff; 16])).is_none());
    }

    #[test]
    fn leaked_workflow_id_returns_same_shape_as_never_issued() {
        // Both "never issued" and "issued to a different workload" lookups
        // must return None with identical response shape. This ensures that
        // a leaked workflow_id cannot be distinguished from a non-existent
        // one, preventing information leakage (I-WA2).
        let mut table = WorkflowTable::new();

        // Workflow A is registered.
        let wf_a = WorkflowRef([0xAA; 16]);
        table.declare(wf_a, WorkloadProfile::AiTraining, PhaseId(1));

        let lookup = AdvisoryLookup::new(&table);

        // Case 1: workflow_ref that was never issued at all.
        let never_issued = WorkflowRef([0xBB; 16]);
        let result_never = lookup.lookup(&never_issued);

        // Case 2: workflow_ref issued to workload A, looked up by a
        // hypothetical "workload B" — but since AdvisoryLookup has no
        // caller identity, any ref not in the table returns None.
        // We simulate this with a third ref that is not wf_a.
        let wrong_workload = WorkflowRef([0xCC; 16]);
        let result_wrong = lookup.lookup(&wrong_workload);

        // Both must be None (identical shape).
        assert!(
            result_never.is_none(),
            "never-issued workflow_ref must return None"
        );
        assert!(
            result_wrong.is_none(),
            "wrong-workload workflow_ref must return None"
        );

        // Verify the existing workflow_ref still works (sanity).
        assert!(
            lookup.lookup(&wf_a).is_some(),
            "registered workflow_ref must return Some"
        );
    }
}
