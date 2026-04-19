//! Step definitions for workflow-advisory.feature.

use cucumber::{given, then, when};
use kiseki_common::advisory::*;

use crate::KisekiWorld;

#[given("a Kiseki cluster with Workflow Advisory enabled cluster-wide")]
async fn given_advisory_enabled(_world: &mut KisekiWorld) {
    // Advisory is active by default in World::new().
}

#[when(regex = r#"^workload "(\S+)" declares a workflow with profile "(\S+)"$"#)]
async fn when_declare_workflow(world: &mut KisekiWorld, _workload: String, _profile: String) {
    let wf_ref = WorkflowRef([0x42; 16]);
    world
        .advisory_table
        .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
}

#[then("the workflow is declared successfully")]
async fn then_declared(world: &mut KisekiWorld) {
    assert!(world.advisory_table.active_count() > 0);
}
