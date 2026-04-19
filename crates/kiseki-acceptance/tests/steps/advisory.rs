//! Step definitions for workflow-advisory.feature.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_advisory::budget::BudgetConfig;
use kiseki_common::advisory::*;

#[given("a Kiseki cluster with Workflow Advisory enabled cluster-wide")]
async fn given_advisory(_w: &mut KisekiWorld) {}

#[when(regex = r#"^workload "(\S+)" declares workflow with profile "(\S+)" phase "(\S+)"$"#)]
async fn when_declare(w: &mut KisekiWorld, workload: String, _profile: String, _phase: String) {
    let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    w.advisory_table
        .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
    w.last_workflow_ref = Some(wf_ref);
    w.workflow_names.insert(workload, wf_ref);
}

#[then("the workflow is declared")]
async fn then_declared(w: &mut KisekiWorld) {
    assert!(w.advisory_table.active_count() > 0);
}

#[when(regex = r#"^the workflow advances to phase (\d+)$"#)]
async fn when_phase(w: &mut KisekiWorld, phase: u64) {
    let wf_ref = w.last_workflow_ref.unwrap();
    let entry = w.advisory_table.get_mut(&wf_ref).unwrap();
    match entry.advance_phase(PhaseId(phase)) {
        Ok(()) => w.last_error = None,
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[then("the phase advances successfully")]
async fn then_phase_ok(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none(), "error: {:?}", w.last_error);
}

#[when(regex = r#"^the workflow tries to go back to phase (\d+)$"#)]
async fn when_phase_back(w: &mut KisekiWorld, phase: u64) {
    let wf_ref = w.last_workflow_ref.unwrap();
    let entry = w.advisory_table.get_mut(&wf_ref).unwrap();
    match entry.advance_phase(PhaseId(phase)) {
        Ok(()) => w.last_error = None,
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[then("the phase advance is rejected as non-monotonic")]
async fn then_non_monotonic(w: &mut KisekiWorld) {
    assert!(w
        .last_error
        .as_ref()
        .is_some_and(|e| e.contains("monotonic")));
}

#[when("the budget limit is exceeded")]
async fn when_budget_exceeded(w: &mut KisekiWorld) {
    // Exhaust hint budget
    for _ in 0..101 {
        let _ = w.budget_enforcer.try_hint();
    }
    match w.budget_enforcer.try_hint() {
        Ok(()) => w.last_error = None,
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[then("the hint is rejected with budget exceeded")]
async fn then_budget(w: &mut KisekiWorld) {
    assert!(w.last_error.as_ref().is_some_and(|e| e.contains("budget")));
}

#[when(regex = r#"^the workflow "(\S+)" is ended$"#)]
async fn when_end(w: &mut KisekiWorld, name: String) {
    if let Some(&wf_ref) = w.workflow_names.get(&name) {
        w.advisory_table.end(&wf_ref);
    }
}

#[then("the workflow is removed from the active table")]
async fn then_removed(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        assert!(w.advisory_table.get(&wf_ref).is_none());
    }
}

// Background
#[given(regex = r#"^organization "(\S+)" with project "(\S+)" and workload "(\S+)"$"#)]
async fn given_org_project_workload(w: &mut KisekiWorld, org: String, _proj: String, _wl: String) {
    w.ensure_tenant(&org);
}
