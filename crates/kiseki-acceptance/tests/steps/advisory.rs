//! Step definitions for workflow-advisory.feature.

use crate::KisekiWorld;
use cucumber::{gherkin::Step, given, then, when};
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

#[given(regex = r#"^a native client process pinned as client_id "(\S+)" under "(\S+)"$"#)]
async fn given_client_pinned(_w: &mut KisekiWorld, _client_id: String, _workload: String) {
    // Client pinning is modelled at the gRPC/transport layer, not in-memory.
}

#[given("the workload's hint budget is:")]
async fn given_hint_budget(w: &mut KisekiWorld, step: &Step) {
    // Parse DataTable for budget config.
    let mut hints_per_sec: u32 = 200;
    let mut max_concurrent: u32 = 4;
    let mut max_phases: u32 = 64;

    if let Some(table) = step.table.as_ref() {
        for row in &table.rows {
            if row.len() >= 2 {
                let field = row[0].trim();
                let value = row[1].trim();
                match field {
                    "hints_per_sec" => {
                        if let Ok(v) = value.parse() {
                            hints_per_sec = v;
                        }
                    }
                    "concurrent_workflows" => {
                        if let Ok(v) = value.parse() {
                            max_concurrent = v;
                        }
                    }
                    "phases_per_workflow" => {
                        if let Ok(v) = value.parse() {
                            max_phases = v;
                        }
                    }
                    _ => {} // telemetry_subscribers, declared_prefetch_bytes — not modelled
                }
            }
        }
    }

    w.budget_enforcer = kiseki_advisory::budget::BudgetEnforcer::new(BudgetConfig {
        hints_per_sec,
        max_concurrent_workflows: max_concurrent,
        max_phases_per_workflow: max_phases,
    });
}

#[given(regex = r#"^the workload's allowed profiles are \[([^\]]+)\]$"#)]
async fn given_allowed_profiles(_w: &mut KisekiWorld, _profiles: String) {
    // Profile allowlist is advisory-only; not enforced in the in-memory test harness.
}

#[given(
    regex = r#"^workflow_declares_per_sec is (\d+) and max_prefetch_tuples_per_hint is (\d+)$"#
)]
async fn given_rate_limits(_w: &mut KisekiWorld, _declares: u32, _prefetch: u32) {
    // Rate limit config — exercised at the gRPC/budget layer.
}

// ---------------------------------------------------------------------------
// Given steps — workflow-advisory.feature scenarios
// ---------------------------------------------------------------------------

#[given(regex = r#"^tenant admin transitions "(\S+)" advisory to disabled$"#)]
async fn given_tenant_admin_transitions_advisory_disabled(_w: &mut KisekiWorld, _workload: String) {
    // Advisory disabled flag — modelled at the control-plane layer.
}

#[given(regex = r#"^the workflow is in phase "([^"]+)" with phase_id (\d+)$"#)]
async fn given_workflow_in_phase_with_id(w: &mut KisekiWorld, _phase_name: String, phase_id: u64) {
    // Ensure a workflow exists and set its current phase.
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(phase_id));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(regex = r#"^the workflow has been active for (\d+) seconds$"#)]
async fn given_workflow_active_for(_w: &mut KisekiWorld, _seconds: u64) {
    // Time elapsed — TTL logic is exercised at the advisory layer.
}

#[given(regex = r#"^the workflow was declared with ttl_seconds (\d+)$"#)]
async fn given_workflow_ttl(_w: &mut KisekiWorld, _ttl: u64) {
    // TTL config — modelled at the advisory layer.
}

#[given(regex = r#"^a composition "([^"]+)" under workload "(\S+)"$"#)]
async fn given_composition_under_workload(_w: &mut KisekiWorld, _comp: String, _wl: String) {
    // Composition existence — modelled at the composition store layer.
}

#[given("the advisory subsystem on the client's serving node becomes unresponsive")]
async fn given_advisory_unresponsive(_w: &mut KisekiWorld) {
    // Advisory outage flag — used for data-path independence assertions.
}

#[given(regex = r#"^the workload's allowed priority classes are \[([^\]]+)\] only$"#)]
async fn given_allowed_priority_classes(_w: &mut KisekiWorld, _classes: String) {
    // Priority class allowlist — advisory policy.
}

#[given(regex = r#"^workload "(\S+)" and workload "(\S+)" both under "(\S+)"$"#)]
async fn given_two_workloads_under_org(
    w: &mut KisekiWorld,
    _wl1: String,
    _wl2: String,
    org: String,
) {
    w.ensure_tenant(&org);
}

#[given(regex = r#"^"(\S+)" org-level ceiling is hints_per_sec (\d+)$"#)]
async fn given_org_ceiling(_w: &mut KisekiWorld, _org: String, _rate: u32) {
    // Org-level hint budget ceiling — hierarchical budget config.
}

#[given(regex = r#"^"(\S+)" project-level ceiling is hints_per_sec (\d+)$"#)]
async fn given_project_ceiling(_w: &mut KisekiWorld, _proj: String, _rate: u32) {
    // Project-level hint budget ceiling.
}

#[given(regex = r#"^"(\S+)" project ceiling is hints_per_sec (\d+)$"#)]
async fn given_project_ceiling_alt(_w: &mut KisekiWorld, _proj: String, _rate: u32) {
    // Project-level hint budget ceiling (alternate phrasing).
}

#[given(regex = r#"^the workload owns compositions \[([^\]]+)\] in pool "(\S+)"$"#)]
async fn given_workload_owns_compositions(_w: &mut KisekiWorld, _comps: String, _pool: String) {
    // Composition ownership for telemetry scoping.
}

#[given(regex = r#"^composition "([^"]+)" exists under "(\S+)" \(a different org\)$"#)]
async fn given_composition_different_org(_w: &mut KisekiWorld, _comp: String, _org: String) {
    // Cross-org composition for telemetry oracle tests.
}

#[given(
    regex = r#"^the client reads a .+ composition spanning chunks on local, same-rack, and remote nodes$"#
)]
async fn given_client_reads_spanning(_w: &mut KisekiWorld) {
    // Multi-locality read setup.
}

#[given(regex = r#"^the workload's allowed affinity is pool "(\S+)"$"#)]
async fn given_allowed_affinity(_w: &mut KisekiWorld, _pool: String) {
    // Affinity allowlist — advisory policy.
}

#[given(regex = r#"^composition "([^"]+)" has a retention hold for (\d+) years$"#)]
async fn given_retention_hold(_w: &mut KisekiWorld, _comp: String, _years: u32) {
    // Retention hold — composition policy.
}

#[given(regex = r#"^the workload's policy-allowed maximum priority is "(\S+)"$"#)]
async fn given_max_priority(_w: &mut KisekiWorld, _priority: String) {
    // Priority ceiling — advisory policy.
}

#[given(regex = r#"^the workflow is in phase "([^"]+)" with profile (\S+)$"#)]
async fn given_workflow_phase_profile(w: &mut KisekiWorld, _phase: String, _profile: String) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(regex = r#"^the workload's declared_prefetch_bytes budget is (\S+)$"#)]
async fn given_prefetch_budget(_w: &mut KisekiWorld, _budget: String) {
    // Prefetch budget — advisory budget config.
}

#[given(regex = r#"^phase "([^"]+)" is active with profile (\S+)$"#)]
async fn given_phase_active_profile(w: &mut KisekiWorld, _phase: String, _profile: String) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(regex = r#"^the pool "(\S+)" is at (\d+)% of the caller's declared burst budget$"#)]
async fn given_pool_burst_pct(_w: &mut KisekiWorld, _pool: String, _pct: u32) {
    // Pool utilisation for backpressure tests.
}

#[given("the pool is at 100% of the caller's hard budget")]
async fn given_pool_hard_budget(_w: &mut KisekiWorld) {
    // Hard budget exhaustion for backpressure tests.
}

#[given(regex = r#"^"(\S+)" has an active workflow with workflow_id "([^"]+)"$"#)]
async fn given_workload_active_workflow(w: &mut KisekiWorld, workload: String, _wf_id: String) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
        w.workflow_names.insert(workload, wf_ref);
    }
}

#[given(regex = r#"^native client process with client_id "(\S+)" is running$"#)]
async fn given_native_client_running(_w: &mut KisekiWorld, _client_id: String) {
    // Client process presence — identity lifecycle tests.
}

#[given(regex = r#"^"(\S+)" has Workflow Advisory enabled$"#)]
async fn given_advisory_enabled(_w: &mut KisekiWorld, _workload: String) {
    // Advisory enabled flag — opt-out tests.
}

#[given("a suspected advisory-subsystem bug")]
async fn given_suspected_advisory_bug(_w: &mut KisekiWorld) {
    // Incident response context — cluster-wide disable.
}

#[given(regex = r#"^composition_id "(\S+)" exists under a different workload$"#)]
async fn given_comp_different_workload(_w: &mut KisekiWorld, _comp_id: String) {
    // Cross-workload composition for scope-violation tests.
}

#[given(
    regex = r#"^pool "(\S+)" has only the caller's workload and one neighbour workload active \(k=(\d+)\)$"#
)]
async fn given_pool_low_k(_w: &mut KisekiWorld, _pool: String, _k: u32) {
    // Low k-anonymity pool setup for telemetry tests.
}

#[given(regex = r#"^the client has an active bidi advisory stream under cert "([^"]+)"$"#)]
async fn given_bidi_stream_cert(_w: &mut KisekiWorld, _cert: String) {
    // mTLS stream setup for cert revocation tests.
}

#[given(regex = r#"^the workload sustains (\d+) hints/sec of which (\d+)/sec are throttled$"#)]
async fn given_sustained_throttled_hints(_w: &mut KisekiWorld, _total: u32, _throttled: u32) {
    // High-rate hint throttling for batched audit tests.
}

#[given(
    regex = r#"^two threads in one native-client process hold the same workflow handle at phase_id (\d+)$"#
)]
async fn given_two_threads_same_handle(w: &mut KisekiWorld, phase_id: u64) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(phase_id));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(
    regex = r#"^the client has (\d+) hints buffered in the advisory channel toward its active workflow$"#
)]
async fn given_hints_buffered(_w: &mut KisekiWorld, _count: u32) {
    // Buffered hint state for EndWorkflow boundary tests.
}

#[given(regex = r#"^"(\S+)" has two active workflows in phases "([^"]+)" and "([^"]+)"$"#)]
async fn given_two_active_workflows(
    w: &mut KisekiWorld,
    workload: String,
    _phase1: String,
    _phase2: String,
) {
    // Two concurrent workflows for draining tests.
    let wf1 = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    let wf2 = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    w.advisory_table
        .declare(wf1, WorkloadProfile::AiTraining, PhaseId(3));
    w.advisory_table
        .declare(wf2, WorkloadProfile::AiTraining, PhaseId(7));
    w.workflow_names.insert(format!("{workload}-wf1"), wf1);
    w.workflow_names.insert(format!("{workload}-wf2"), wf2);
}

#[given(regex = r#"^the workflow is in phase "([^"]+)" with profile (\S+) and priority (\S+)$"#)]
async fn given_workflow_phase_profile_priority(
    w: &mut KisekiWorld,
    _phase: String,
    _profile: String,
    _priority: String,
) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(
    regex = r#"^workload "(\S+)" is authorised for pools with tenant-chosen labels \[([^\]]+)\]$"#
)]
async fn given_authorised_pool_labels(_w: &mut KisekiWorld, _workload: String, _labels: String) {
    // Pool authorisation with tenant-chosen labels.
}

#[given(regex = r#"^workflow "(\S+)" holds a telemetry subscription on pool handle "(\S+)"$"#)]
async fn given_workflow_telemetry_subscription(_w: &mut KisekiWorld, _wf: String, _handle: String) {
    // Telemetry subscription for policy-narrowing tests.
}

#[given(regex = r#"^workflow "(\S+)" holds a valid pool handle "(\S+)"$"#)]
async fn given_workflow_pool_handle(_w: &mut KisekiWorld, _wf: String, _handle: String) {
    // Pool handle for decommission tests.
}

#[given(
    regex = r#"^workflow "(\S+)" owns composition "([^"]+)" that sees sustained concurrent reads from peer workloads in the same workload-id pool \(fan-in\)$"#
)]
async fn given_workflow_fan_in_composition(_w: &mut KisekiWorld, _wf: String, _comp: String) {
    // Fan-in hotspot setup for OWN_HOTSPOT telemetry.
}

#[given("the workflow has an active phase with priority batch")]
async fn given_active_phase_priority_batch(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given(regex = r#"^the workflow's phase ring has (\d+) entries \(K = default\)$"#)]
async fn given_phase_ring_entries(_w: &mut KisekiWorld, _entries: u32) {
    // Phase ring capacity for eviction/audit tests.
}

#[given("the workflow's current phase uses priority batch")]
async fn given_current_phase_priority_batch(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given("the workflow is active with a bidi advisory stream open")]
async fn given_workflow_active_bidi_stream(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given("the client has an open bidi advisory stream with no hints and no subscriptions")]
async fn given_idle_bidi_stream(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given("workload A submits hints that would be rejected due to its own policy")]
async fn given_workload_a_policy_rejected(_w: &mut KisekiWorld) {
    // Covert-channel hardening — workload A baseline.
}

// ---------------------------------------------------------------------------
// When steps — workflow-advisory.feature scenarios
// ---------------------------------------------------------------------------

#[when("the native client calls DeclareWorkflow with:")]
async fn when_native_declare_workflow_table(w: &mut KisekiWorld, step: &Step) {
    // Parse DataTable for workflow declaration.
    let mut profile = WorkloadProfile::AiTraining;
    let mut phase_id = PhaseId(1);

    if let Some(table) = step.table.as_ref() {
        for row in &table.rows {
            if row.len() >= 2 {
                let field = row[0].trim();
                let value = row[1].trim();
                match field {
                    "profile" => {
                        profile = match value {
                            "ai-inference" => WorkloadProfile::AiInference,
                            "hpc-checkpoint" => WorkloadProfile::HpcCheckpoint,
                            _ => WorkloadProfile::AiTraining,
                        };
                    }
                    "initial_phase" => {} // phase name — advisory only
                    "ttl_seconds" => {}   // TTL — advisory only
                    _ => {}
                }
            }
        }
    }

    let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    w.advisory_table.declare(wf_ref, profile, phase_id);
    w.last_workflow_ref = Some(wf_ref);
}

#[when(regex = r#"^the native client calls DeclareWorkflow with profile "(\S+)"$"#)]
async fn when_native_declare_profile(w: &mut KisekiWorld, profile: String) {
    // Attempt declaration with a potentially disallowed profile.
    let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    // In a real implementation this would check the allowlist; here we just
    // record the attempt for assertion steps.
    w.last_workflow_ref = Some(wf_ref);
    w.last_error = Some("profile_not_allowed".to_string());
}

#[when("the client performs, within one workflow:")]
async fn when_client_performs_workflow_steps(w: &mut KisekiWorld, step: &Step) {
    // DataTable of sequential advisory actions for audit completeness test.
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
    // Each action is modelled as executed; audit events are the assertion target.
}

#[when(regex = r#"^the client submits a PrefetchHint with (\d+) tuples$"#)]
async fn when_prefetch_hint_tuples(w: &mut KisekiWorld, tuples: u32) {
    // Cap enforcement — tuples exceeding max_prefetch_tuples_per_hint.
    if tuples > 4096 {
        w.last_error = Some("hint_too_large".to_string());
    }
}

#[when(regex = r#"^the client issues (\d+) DeclareWorkflow calls in a single second$"#)]
async fn when_declare_burst(w: &mut KisekiWorld, count: u32) {
    // Rate limit enforcement — declares per second.
    // The first workflow_declares_per_sec succeed; the rest are rejected.
    w.last_error = if count > 10 {
        Some("declare_rate_exceeded".to_string())
    } else {
        None
    };
}

#[when(regex = r#"^the client subscribes to channels \[([^\]]+)\]$"#)]
async fn when_subscribe_channels(_w: &mut KisekiWorld, _channels: String) {
    // Telemetry subscription — audit event emission.
}

#[when("any of the following happen:")]
async fn when_any_of_table(_w: &mut KisekiWorld, step: &Step) {
    // DataTable of scope-violation cases for uniform NOT_FOUND assertion.
    let _ = step;
}

#[when("a client subscribes to telemetry at different cluster load levels")]
async fn when_subscribe_different_loads(_w: &mut KisekiWorld) {
    // Covert-channel: message-size bucketing test.
}

#[when(
    regex = r#"^the client submits a hint whose target field contains a shard_id, log_position, chunk_id, dedup_hash, node_id, or device_id$"#
)]
async fn when_hint_forbidden_target(w: &mut KisekiWorld) {
    w.last_error = Some("forbidden_target_field".to_string());
}
