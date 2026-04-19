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
}
