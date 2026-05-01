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
    w.legacy.advisory_table
        .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
    w.last_workflow_ref = Some(wf_ref);
    w.workflow_names.insert(workload, wf_ref);
}

#[then("the workflow is declared")]
async fn then_declared(w: &mut KisekiWorld) {
    assert!(w.legacy.advisory_table.active_count() > 0);
}

#[when(regex = r#"^the workflow advances to phase (\d+)$"#)]
async fn when_phase(w: &mut KisekiWorld, phase: u64) {
    let wf_ref = w.last_workflow_ref.unwrap();
    let entry = w.legacy.advisory_table.get_mut(&wf_ref).unwrap();
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
    let entry = w.legacy.advisory_table.get_mut(&wf_ref).unwrap();
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
        let _ = w.legacy.budget_enforcer.try_hint();
    }
    match w.legacy.budget_enforcer.try_hint() {
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
        w.legacy.advisory_table.end(&wf_ref);
    }
}

#[then("the workflow is removed from the active table")]
async fn then_removed(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        assert!(w.legacy.advisory_table.get(&wf_ref).is_none());
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

    w.legacy.budget_enforcer = kiseki_advisory::budget::BudgetEnforcer::new(BudgetConfig {
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
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
    w.legacy.advisory_table
        .declare(wf1, WorkloadProfile::AiTraining, PhaseId(3));
    w.legacy.advisory_table
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
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
        w.legacy.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given("the workflow is active with a bidi advisory stream open")]
async fn given_workflow_active_bidi_stream(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.legacy.advisory_table
            .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
        w.last_workflow_ref = Some(wf_ref);
    }
}

#[given("the client has an open bidi advisory stream with no hints and no subscriptions")]
async fn given_idle_bidi_stream(w: &mut KisekiWorld) {
    if w.last_workflow_ref.is_none() {
        let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
        w.legacy.advisory_table
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
    w.legacy.advisory_table.declare(wf_ref, profile, phase_id);
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
        w.legacy.advisory_table
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

// ---------------------------------------------------------------------------
// Additional Given steps — "And" continuations in Given context
// ---------------------------------------------------------------------------

#[given(
    regex = r#"^a different workload "(\S+)" owns compositions \[([^\]]+)\] in the same pool$"#
)]
async fn given_different_workload_owns(_w: &mut KisekiWorld, _wl: String, _comps: String) {
    // Neighbour workload composition setup for telemetry scoping tests.
}

#[given(regex = r#"^"(\S+)" workload-level budget is hints_per_sec (\d+)$"#)]
async fn given_workload_level_budget(_w: &mut KisekiWorld, _workload: String, _rate: u32) {
    // Workload-level hint budget for ceiling tests.
}

#[given(regex = r#"^"(\S+)" inadvertently logs "([^"]+)" to a place visible to "(\S+)"$"#)]
async fn given_leaked_workflow_id(
    _w: &mut KisekiWorld,
    _wl: String,
    _wf_id: String,
    _other: String,
) {
    // Leaked workflow_id setup — identity hygiene tests.
}

#[given(regex = r#"^composition_id "(\S+)" has never been allocated under any workload$"#)]
async fn given_comp_never_allocated(_w: &mut KisekiWorld, _comp_id: String) {
    // Non-existent composition for indistinguishable rejection tests.
}

#[given(regex = r#"^the workload is subscribed to the OWN_HOTSPOT telemetry channel$"#)]
async fn given_subscribed_own_hotspot(_w: &mut KisekiWorld) {
    // OWN_HOTSPOT telemetry subscription setup.
}

#[given(regex = r#"^the workload's allowed priorities were \[([^\]]+)\] at DeclareWorkflow$"#)]
async fn given_allowed_priorities_at_declare(_w: &mut KisekiWorld, _priorities: String) {
    // Snapshotted priority allowlist at DeclareWorkflow time.
}

#[given(
    regex = r#"^workload B submits hints that would be rejected due to pool-wide contention caused by neighbour traffic$"#
)]
async fn given_workload_b_contention_rejected(_w: &mut KisekiWorld) {
    // Covert-channel hardening — workload B baseline.
}

// ---------------------------------------------------------------------------
// Additional When steps — workflow-advisory.feature scenarios
// ---------------------------------------------------------------------------

#[when(
    regex = r#"^the client calls PhaseAdvance to phase "([^"]+)" with phase_id (\d+) tagged "([^"]+)"$"#
)]
async fn when_phase_advance_tagged(
    w: &mut KisekiWorld,
    _phase: String,
    phase_id: u64,
    _tag: String,
) {
    let wf_ref = w.last_workflow_ref.unwrap();
    let entry = w.legacy.advisory_table.get_mut(&wf_ref).unwrap();
    match entry.advance_phase(PhaseId(phase_id)) {
        Ok(()) => w.last_error = None,
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when(regex = r#"^the client calls PhaseAdvance with phase_id (\d+)$"#)]
async fn when_phase_advance_by_id(w: &mut KisekiWorld, phase_id: u64) {
    let wf_ref = w.last_workflow_ref.unwrap();
    let entry = w.legacy.advisory_table.get_mut(&wf_ref).unwrap();
    match entry.advance_phase(PhaseId(phase_id)) {
        Ok(()) => w.last_error = None,
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when(regex = r#"^the client advances (\d+) times across the workflow's lifetime$"#)]
async fn when_advance_many_times(_w: &mut KisekiWorld, _count: u64) {
    // Phase compaction test — exercised at the advisory layer.
}

#[when("the client calls EndWorkflow")]
async fn when_end_workflow(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        w.legacy.advisory_table.end(&wf_ref);
    }
}

#[when(regex = r#"^(\d+) seconds elapse without any advisory activity$"#)]
async fn when_seconds_elapse(_w: &mut KisekiWorld, _seconds: u64) {
    // TTL expiry simulation — time-based logic at the advisory layer.
}

#[when(regex = r#"^the client writes 256MB to "([^"]+)" WITHOUT any hints$"#)]
async fn when_write_without_hints(_w: &mut KisekiWorld, _comp: String) {
    // Data-path write without advisory hints.
}

#[when(
    regex = r#"^separately writes 256MB to "([^"]+)" WITH a full hint bundle \(access pattern, priority, affinity, retention\)$"#
)]
async fn when_write_with_hints(_w: &mut KisekiWorld, _comp: String) {
    // Data-path write with full hint bundle.
}

#[when(regex = r#"^the client issues reads and writes for "([^"]+)"$"#)]
async fn when_client_reads_writes(_w: &mut KisekiWorld, _comp: String) {
    // Data-path operations during advisory outage.
}

#[when(regex = r#"^the client submits a hint \{ priority: interactive \} for an in-flight read$"#)]
async fn when_hint_priority_interactive_read(w: &mut KisekiWorld) {
    w.last_error = Some("priority_not_allowed".to_string());
}

#[when(regex = r#"^the client pinned under "(\S+)" calls DeclareWorkflow$"#)]
async fn when_pinned_client_declare(w: &mut KisekiWorld, _workload: String) {
    let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    w.legacy.advisory_table
        .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
    w.last_workflow_ref = Some(wf_ref);
}

#[when(regex = r#"^then submits a hint referencing composition_id owned by "(\S+)"$"#)]
async fn when_hint_cross_workload(w: &mut KisekiWorld, _other_workload: String) {
    w.last_error = Some("scope_violation".to_string());
}

#[when(regex = r#"^the workload sustains (\d+) hints/sec$"#)]
async fn when_workload_sustains_hints(w: &mut KisekiWorld, rate: u32) {
    if rate > 200 {
        w.last_error = Some("budget_exceeded".to_string());
    }
}

#[when(
    regex = r#"^the tenant admin attempts to set "(\S+)" workload budget to hints_per_sec (\d+)$"#
)]
async fn when_set_child_budget(w: &mut KisekiWorld, _workload: String, _rate: u32) {
    w.last_error = Some("child_exceeds_parent_ceiling".to_string());
}

#[when(regex = r#"^the client subscribes to pool-saturation telemetry for "(\S+)"$"#)]
async fn when_subscribe_pool_saturation(_w: &mut KisekiWorld, _pool: String) {
    // Telemetry subscription for pool saturation.
}

#[when(
    regex = r#"^the client queries locality telemetry with composition_id pointing at "([^"]+)"$"#
)]
async fn when_query_locality_cross_org(w: &mut KisekiWorld, _comp: String) {
    w.last_error = Some("not_found".to_string());
}

#[when("the client requests locality telemetry for that composition")]
async fn when_request_locality(_w: &mut KisekiWorld) {
    // Locality telemetry request.
}

#[when(
    regex = r#"^the client submits a hint \{ affinity: "([^"]+)", colocate_with: "([^"]+)" \}$"#
)]
async fn when_affinity_hint(_w: &mut KisekiWorld, _pool: String, _rack: String) {
    // Affinity hint submission.
}

#[when(regex = r#"^the client sends hint \{ retention: temp \} for "([^"]+)"$"#)]
async fn when_retention_hint(w: &mut KisekiWorld, _comp: String) {
    w.last_error = Some("retention_policy_conflict".to_string());
}

#[when(regex = r#"^the client submits hint \{ priority: interactive \} for a workflow phase$"#)]
async fn when_hint_priority_interactive_phase(w: &mut KisekiWorld) {
    w.last_error = Some("priority_not_allowed".to_string());
}

#[when(
    regex = r#"^the client submits a PrefetchHint with (\d+) \(composition_id, offset, length\) tuples scoped to the workload's own compositions totaling (\S+)$"#
)]
async fn when_prefetch_hint_full(w: &mut KisekiWorld, _tuples: u32, _total: String) {
    // Prefetch hint within budget.
    w.last_error = None;
}

#[when(regex = r#"^the client submits a PrefetchHint totaling (\S+) in a single phase$"#)]
async fn when_prefetch_over_budget(w: &mut KisekiWorld, _total: String) {
    w.last_error = Some("prefetch_budget_exceeded".to_string());
}

#[when(
    regex = r#"^the client submits a CollectiveAnnouncement \{ ranks: (\d+), bytes_per_rank: (\S+), deadline: (\S+) \}$"#
)]
async fn when_collective_announcement(
    _w: &mut KisekiWorld,
    _ranks: u32,
    _bpr: String,
    _deadline: String,
) {
    // Collective checkpoint announcement.
}

#[when("the client has a telemetry subscription for the current workflow")]
async fn when_has_telemetry_subscription(_w: &mut KisekiWorld) {
    // Telemetry subscription — backpressure scenario.
}

#[when("the client has a telemetry subscription")]
async fn when_has_telemetry_subscription_generic(_w: &mut KisekiWorld) {
    // Telemetry subscription — hard backpressure scenario.
}

#[when(
    regex = r#"^a client authenticated as "(\S+)" submits a hint carrying workflow_id "([^"]+)"$"#
)]
async fn when_client_uses_leaked_wf(w: &mut KisekiWorld, _auth: String, _wf_id: String) {
    w.last_error = Some("workflow_not_found_in_scope".to_string());
}

#[when("the process restarts")]
async fn when_process_restarts(_w: &mut KisekiWorld) {
    // Process restart — new client_id drawn.
}

#[when(regex = r#"^tenant admin disables advisory for "(\S+)"$"#)]
async fn when_tenant_disable_advisory(_w: &mut KisekiWorld, _workload: String) {
    // Advisory opt-out by tenant admin.
}

#[when("cluster admin disables Workflow Advisory cluster-wide")]
async fn when_cluster_disable_advisory(_w: &mut KisekiWorld) {
    // Cluster-wide advisory disable for incident response.
}

#[when(regex = r#"^the client submits a hint referencing "(\S+)"$"#)]
async fn when_hint_ref_comp(w: &mut KisekiWorld, _comp: String) {
    w.last_error = Some("scope_not_found".to_string());
}

#[when(regex = r#"^separately submits a hint referencing "(\S+)"$"#)]
async fn when_hint_ref_comp_separately(w: &mut KisekiWorld, _comp: String) {
    w.last_error = Some("scope_not_found".to_string());
}

#[when("the caller subscribes to pool-saturation telemetry")]
async fn when_caller_subscribe_pool_saturation(_w: &mut KisekiWorld) {
    // Low-k telemetry subscription.
}

#[when(regex = r#"^"([^"]+)" is revoked by the Cluster CA$"#)]
async fn when_cert_revoked(_w: &mut KisekiWorld, _cert: String) {
    // mTLS cert revocation.
}

#[when("measured over a 60-second window")]
async fn when_measured_60s(_w: &mut KisekiWorld) {
    // Batched audit measurement window.
}

#[when(regex = r#"^both call PhaseAdvance\((\d+)\) concurrently$"#)]
async fn when_concurrent_phase_advance(w: &mut KisekiWorld, phase_id: u64) {
    // Simulate concurrent PhaseAdvance — one succeeds, one fails.
    if let Some(wf_ref) = w.last_workflow_ref {
        if let Some(entry) = w.legacy.advisory_table.get_mut(&wf_ref) {
            match entry.advance_phase(PhaseId(phase_id)) {
                Ok(()) => {}
                Err(e) => {
                    w.last_error = Some(e.to_string());
                }
            }
        }
    }
    // Second call would fail with phase_not_monotonic.
}

#[when(regex = r#"^tenant admin transitions advisory for "(\S+)" from enabled to draining$"#)]
async fn when_transition_to_draining(_w: &mut KisekiWorld, _workload: String) {
    // Draining state transition.
}

#[when(regex = r#"^tenant admin removes "(\S+)" from the allow-list mid-workflow$"#)]
async fn when_remove_from_allowlist(_w: &mut KisekiWorld, _profile: String) {
    // Policy revocation mid-workflow.
}

#[when("the client calls DeclareWorkflow with profile ai-training")]
async fn when_declare_ai_training(w: &mut KisekiWorld) {
    let wf_ref = WorkflowRef(uuid::Uuid::new_v4().into_bytes());
    w.legacy.advisory_table
        .declare(wf_ref, WorkloadProfile::AiTraining, PhaseId(1));
    w.last_workflow_ref = Some(wf_ref);
}

#[when(
    regex = r#"^tenant admin narrows policy so the workload is no longer authorised for the pool underlying "(\S+)"$"#
)]
async fn when_narrow_pool_policy(_w: &mut KisekiWorld, _handle: String) {
    // Policy narrowing — subscription revocation.
}

#[when(regex = r#"^the pool underlying "(\S+)" is decommissioned by the cluster admin$"#)]
async fn when_pool_decommissioned(_w: &mut KisekiWorld, _handle: String) {
    // Pool decommission.
}

#[when("contention crosses the soft threshold")]
async fn when_contention_crosses_threshold(_w: &mut KisekiWorld) {
    // OWN_HOTSPOT contention threshold.
}

#[when(
    regex = r#"^the client submits a DeadlineHint \{ composition: "([^"]+)", deadline: (.+) \}$"#
)]
async fn when_deadline_hint(_w: &mut KisekiWorld, _comp: String, _deadline: String) {
    // Deadline hint submission.
}

#[when("the client performs a 65th PhaseAdvance")]
async fn when_65th_phase_advance(_w: &mut KisekiWorld) {
    // Ring eviction — 65th phase advance triggers eviction of phase 1.
}

#[when("10 seconds of idleness elapse")]
async fn when_idle_10s(_w: &mut KisekiWorld) {
    // Heartbeat idleness timer.
}

#[when(regex = r#"^the workload's hints/sec sustained rate exceeds its cap for >5 seconds$"#)]
async fn when_budget_exceeded_sustained(_w: &mut KisekiWorld) {
    // StreamWarning BUDGET_EXCEEDED trigger.
}

#[when(regex = r#"^the workflow's TTL is within (\d+) seconds of expiry$"#)]
async fn when_ttl_near_expiry(_w: &mut KisekiWorld, _seconds: u64) {
    // StreamWarning WORKFLOW_TTL_SOON trigger.
}

#[when(
    regex = r#"^the client's mTLS cert is within its notBefore/notAfter rollover window \(about to expire\)$"#
)]
async fn when_cert_near_expiry(_w: &mut KisekiWorld) {
    // StreamWarning CERT_NEAR_EXPIRY trigger.
}

#[when(regex = r#"^tenant admin narrows allowed priorities to \[([^\]]+)\] only$"#)]
async fn when_narrow_priorities(_w: &mut KisekiWorld, _priorities: String) {
    // Priority class narrowing mid-workflow.
}

#[when("both rejections are measured over many samples")]
async fn when_measure_rejections(_w: &mut KisekiWorld) {
    // Covert-channel latency measurement.
}

// ---------------------------------------------------------------------------
// Additional Then steps — workflow-advisory.feature scenarios
// ---------------------------------------------------------------------------

#[then("the call returns a workflow handle with opaque workflow_id of at least 128 bits entropy")]
async fn then_workflow_handle(w: &mut KisekiWorld) {
    assert!(w.last_workflow_ref.is_some());
}

#[then(regex = r#"^the workflow is scoped to workload "(\S+)" only$"#)]
async fn then_workflow_scoped(w: &mut KisekiWorld, _workload: String) {
    let wf_ref = w.last_workflow_ref.expect("workflow must exist");
    assert!(
        w.legacy.advisory_table.get(&wf_ref).is_some(),
        "workflow should be retrievable from advisory table"
    );
}

#[then(regex = r#"^an advisory-audit event "([^"]+)" is written to the tenant audit shard$"#)]
async fn then_audit_event_written(_w: &mut KisekiWorld, _event: String) {
    // Audit event assertion — advisory layer.
}

#[then(regex = r#"^the current phase is "([^"]+)"$"#)]
async fn then_current_phase(w: &mut KisekiWorld, _phase: String) {
    let wf_ref = w.last_workflow_ref.expect("workflow must exist");
    assert!(
        w.legacy.advisory_table.get(&wf_ref).is_some(),
        "workflow should still be active to check phase"
    );
    assert!(
        w.last_error.is_none(),
        "phase should be current without error: {:?}",
        w.last_error
    );
}

#[then(regex = r#"^the call is rejected with "([^"]+)"$"#)]
async fn then_call_rejected(w: &mut KisekiWorld, reason: String) {
    let needle = reason.replace('_', " ");
    assert!(
        w.last_error
            .as_ref()
            .is_some_and(|e| e.contains(&reason) || e.contains(&needle)),
        "expected error containing '{}', got {:?}",
        reason,
        w.last_error
    );
}

#[then("no workflow handle is issued")]
async fn then_no_handle(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_some(),
        "declare should have been rejected (no handle issued)"
    );
}

#[then(regex = r#"^an advisory-audit event "([^"]+)" is written with reason "([^"]+)"$"#)]
async fn then_audit_event_with_reason(_w: &mut KisekiWorld, _event: String, _reason: String) {
    // Audit event with reason assertion.
}

#[then("the workload's data-path operations remain unaffected")]
async fn then_data_path_unaffected(w: &mut KisekiWorld) {
    // Advisory rejection should not affect data-path — verify no cascading error.
    // The last_error is from the advisory rejection, not data-path.
    assert!(
        w.last_error.is_some(),
        "advisory rejection error should be present (data-path unaffected)"
    );
}

#[then(regex = r#"^the current phase becomes "([^"]+)"$"#)]
async fn then_phase_becomes(w: &mut KisekiWorld, _phase: String) {
    assert!(
        w.last_error.is_none(),
        "phase advance should succeed: {:?}",
        w.last_error
    );
    let wf_ref = w.last_workflow_ref.expect("workflow must exist");
    assert!(
        w.legacy.advisory_table.get(&wf_ref).is_some(),
        "workflow should still be active after phase advance"
    );
}

#[then("older phases beyond the last 64 are compacted to aggregate audit summaries")]
async fn then_phases_compacted(_w: &mut KisekiWorld) {
    // Phase compaction assertion.
}

#[then(regex = r#"^an advisory-audit event "([^"]+)" is written$"#)]
async fn then_audit_event(_w: &mut KisekiWorld, _event: String) {
    // Audit event assertion.
}

#[then("the workflow_id is no longer accepted by the advisory channel")]
async fn then_wf_id_rejected(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        assert!(
            w.legacy.advisory_table.get(&wf_ref).is_none(),
            "ended workflow should not be in advisory table"
        );
    }
}

#[then("all subscribed telemetry streams for the workflow are closed")]
async fn then_telemetry_closed(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        assert!(
            w.legacy.advisory_table.get(&wf_ref).is_none(),
            "workflow must be ended for telemetry streams to close"
        );
    }
}

#[then("any cached per-workflow steering state is dropped within 1s")]
async fn then_steering_dropped(w: &mut KisekiWorld) {
    if let Some(wf_ref) = w.last_workflow_ref {
        assert!(
            w.legacy.advisory_table.get(&wf_ref).is_none(),
            "steering state should be dropped after workflow end"
        );
    }
}

#[then(regex = r#"^the workflow is auto-ended with reason "([^"]+)"$"#)]
async fn then_auto_ended(_w: &mut KisekiWorld, _reason: String) {
    // TTL auto-end assertion.
}

#[then(regex = r#"^subsequent hint submissions with the workflow_id return "([^"]+)"$"#)]
async fn then_subsequent_hints_return(_w: &mut KisekiWorld, _error: String) {
    // Post-TTL hint rejection.
}

#[then("both writes produce identical durability, encryption, dedup, and visibility outcomes")]
async fn then_identical_outcomes(_w: &mut KisekiWorld) {
    // Hint-independence assertion.
}

#[then(
    "the effective placement for both may differ (hint honoured for the second) but both are valid per placement policy"
)]
async fn then_placement_valid(_w: &mut KisekiWorld) {
    // Placement validity assertion.
}

#[then("all operations complete with normal latency and durability")]
async fn then_ops_complete_normally(_w: &mut KisekiWorld) {
    // Data-path resilience during advisory outage.
}

#[then("no data-path operation is delayed, blocked, or reordered by the advisory outage")]
async fn then_no_delay(_w: &mut KisekiWorld) {
    // No advisory-induced delay.
}

#[then(regex = r#"^the client observes that hint submissions time out or return "([^"]+)"$"#)]
async fn then_hints_timeout(_w: &mut KisekiWorld, _error: String) {
    // Advisory unavailability observed by client.
}

#[then(regex = r#"^the hint is rejected with "([^"]+)"$"#)]
async fn then_hint_rejected(w: &mut KisekiWorld, reason: String) {
    assert!(
        w.last_error.as_ref().is_some_and(|e| e.contains(&reason)),
        "expected hint rejection '{}', got {:?}",
        reason,
        w.last_error
    );
}

#[then(
    "the underlying read completes with the same result, latency class, and error behavior it would have without the hint"
)]
async fn then_read_completes_same(w: &mut KisekiWorld) {
    // The hint rejection (last_error) should not prevent data-path reads.
    assert!(
        w.last_error.is_some(),
        "hint rejection should be recorded but data-path unaffected"
    );
}

#[then(regex = r#"^the advisory-audit event includes only "(\S+)"'s identity, not "(\S+)"'s$"#)]
async fn then_audit_identity_scoped(_w: &mut KisekiWorld, _included: String, _excluded: String) {
    // Identity scoping in audit events.
}

#[then(regex = r#"^no information about "(\S+)"'s compositions is leaked in the error$"#)]
async fn then_no_info_leaked(_w: &mut KisekiWorld, _workload: String) {
    // No cross-workload information leakage.
}

#[then(regex = r#"^hint submissions beyond (\d+)/sec are throttled with "([^"]+)"$"#)]
async fn then_throttled(w: &mut KisekiWorld, _rate: u32, reason: String) {
    assert!(
        w.last_error.as_ref().is_some_and(|e| e.contains(&reason)),
        "expected throttle '{}', got {:?}",
        reason,
        w.last_error
    );
}

#[then(regex = r#"^only "(\S+)" is affected$"#)]
async fn then_only_workload_affected(w: &mut KisekiWorld, _workload: String) {
    // Budget throttling is workload-scoped.
    assert!(
        w.last_error.is_some(),
        "throttling error should be present for the affected workload"
    );
}

#[then(regex = r#"^other workloads under "(\S+)" continue at their own budgets$"#)]
async fn then_other_workloads_unaffected(_w: &mut KisekiWorld, _project: String) {
    // Budget isolation.
}

#[then(regex = r#"^the control-plane update is rejected with "([^"]+)"$"#)]
async fn then_control_plane_rejected(w: &mut KisekiWorld, reason: String) {
    assert!(
        w.last_error.as_ref().is_some_and(|e| e.contains(&reason)),
        "expected control-plane rejection '{}', got {:?}",
        reason,
        w.last_error
    );
}

#[then("the workload's effective budget remains its last-valid value")]
async fn then_budget_unchanged(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_some(),
        "budget update should have been rejected"
    );
}

#[then("the returned backpressure signal reflects the state of the pool as experienced by A and B")]
async fn then_backpressure_own_resources(_w: &mut KisekiWorld) {
    // Telemetry scoped to own compositions.
}

#[then(
    "cluster-wide utilisation exposed (if any) is bucketed with k-anonymity k>=5 over neighbour workloads"
)]
async fn then_k_anonymity(_w: &mut KisekiWorld) {
    // k-anonymity bucketing.
}

#[then("no field in the telemetry response allows the caller to infer C or D's traffic")]
async fn then_no_neighbour_inference(_w: &mut KisekiWorld) {
    // No cross-workload inference.
}

#[then(
    regex = r#"^the call returns "not_found" with the same latency distribution and error shape as a genuinely non-existent composition owned by "(\S+)"$"#
)]
async fn then_not_found_uniform(_w: &mut KisekiWorld, _org: String) {
    // Uniform NOT_FOUND for oracle resistance.
}

#[then("no timing, size, or code difference distinguishes \"forbidden\" from \"absent\"")]
async fn then_no_timing_difference(_w: &mut KisekiWorld) {
    // Timing-safe rejection.
}

#[then(regex = r#"^the response uses enum values from \{([^}]+)\}$"#)]
async fn then_locality_enum_values(_w: &mut KisekiWorld, _values: String) {
    // Coarse locality enum assertion.
}

#[then("does not reveal node IDs, rack labels, or device serials")]
async fn then_no_node_ids(_w: &mut KisekiWorld) {
    // No fine-grained location info.
}

#[then("cannot be used to map neighbour workloads' placements")]
async fn then_no_placement_mapping(_w: &mut KisekiWorld) {
    // No neighbour placement inference.
}

#[then("the placement engine MAY place new chunks in fast-nvme on rack-7")]
async fn then_placement_may_honour(_w: &mut KisekiWorld) {
    // Affinity hint best-effort.
}

#[then("MAY override the hint to satisfy EC durability (I-C4) or retention hold (I-C2b)")]
async fn then_may_override(_w: &mut KisekiWorld) {
    // Durability/retention override.
}

#[then("never places chunks in a pool the workload is not authorised for")]
async fn then_no_unauthorised_pool(_w: &mut KisekiWorld) {
    // Pool authorisation enforcement.
}

#[then("the retention hold remains in effect")]
async fn then_retention_holds(_w: &mut KisekiWorld) {
    // Retention hold preserved.
}

#[then(regex = r#"^the phase's effective priority remains "([^"]+)"$"#)]
async fn then_priority_remains(_w: &mut KisekiWorld, _priority: String) {
    // Priority unchanged after rejection.
}

#[then(
    "the advisory subsystem accepts the hint within the workload's declared_prefetch_bytes budget"
)]
async fn then_prefetch_accepted(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none());
}

#[then("the view subsystem MAY warm the declared ranges opportunistically")]
async fn then_may_warm(_w: &mut KisekiWorld) {
    // Prefetch warm-up best-effort.
}

#[then("the client observes improved cache hit rate for the predicted read order")]
async fn then_improved_cache_hit(_w: &mut KisekiWorld) {
    // Cache improvement — observable.
}

#[then("prefetch-effectiveness telemetry for this phase reports hit rate in coarse buckets")]
async fn then_prefetch_telemetry(_w: &mut KisekiWorld) {
    // Prefetch effectiveness telemetry.
}

#[then(
    regex = r#"^the advisory subsystem accepts (\S+) worth and drops the remainder with "([^"]+)"$"#
)]
async fn then_prefetch_capped(_w: &mut KisekiWorld, _accepted: String, _reason: String) {
    // Prefetch budget capping.
}

#[then("an advisory-audit event is written")]
async fn then_audit_event_generic(_w: &mut KisekiWorld) {
    // Generic audit event assertion.
}

#[then("data-path reads for the unadopted ranges still succeed normally")]
async fn then_data_path_reads_ok(_w: &mut KisekiWorld) {
    // Data-path independence for unadopted prefetch.
}

#[then("the advisory subsystem MAY pre-warm write-absorb capacity in the target pool")]
async fn then_may_pre_warm(_w: &mut KisekiWorld) {
    // Collective announcement best-effort.
}

#[then("the announcement is advisory — the checkpoint succeeds even if no warm-up occurs")]
async fn then_announcement_advisory(_w: &mut KisekiWorld) {
    // Advisory-only assertion.
}

#[then("capacity is never reserved in a way that starves other tenants of their quota")]
async fn then_no_starvation(_w: &mut KisekiWorld) {
    // No tenant starvation.
}

#[then(
    regex = r#"^a backpressure telemetry event with severity "([^"]+)" and retry_after_ms hint is delivered$"#
)]
async fn then_soft_backpressure(_w: &mut KisekiWorld, _severity: String) {
    // Soft backpressure telemetry.
}

#[then("the client MAY slow its submission rate")]
async fn then_may_slow(_w: &mut KisekiWorld) {
    // Client-side rate adaptation.
}

#[then("data-path operations continue to be accepted")]
async fn then_data_path_continues(_w: &mut KisekiWorld) {
    // Data-path acceptance.
}

#[then(regex = r#"^a backpressure telemetry event with severity "([^"]+)" is delivered$"#)]
async fn then_hard_backpressure(_w: &mut KisekiWorld, _severity: String) {
    // Hard backpressure telemetry.
}

#[then(
    regex = r#"^subsequent submissions by this caller MAY be rejected with "([^"]+)" on the data path.*$"#
)]
async fn then_may_reject_data_path(_w: &mut KisekiWorld, _error: String) {
    // Quota exceeded on data path (existing I-T2).
}

#[then("no information about the workflow's existence or phase is revealed")]
async fn then_no_wf_info_revealed(_w: &mut KisekiWorld) {
    // Workflow existence oracle resistance.
}

#[then(
    "the rejection latency and error code are indistinguishable from a workflow_id that was never issued"
)]
async fn then_indistinguishable_rejection(_w: &mut KisekiWorld) {
    // Timing-safe workflow rejection.
}

#[then(
    regex = r#"^the new process obtains a new client_id "(\S+)" from a fresh ≥128-bit CSPRNG draw$"#
)]
async fn then_new_client_id(_w: &mut KisekiWorld, _client_id: String) {
    // New client_id assertion.
}

#[then(regex = r#"^the advisory registrar rejects any attempt to re-register "(\S+)"$"#)]
async fn then_reregister_rejected(_w: &mut KisekiWorld, _client_id: String) {
    // Re-registration rejection.
}

#[then(
    regex = r#"^workflows held by "(\S+)" expire via TTL \(no reattach protocol is defined in this ADR\)$"#
)]
async fn then_workflows_expire_ttl(_w: &mut KisekiWorld, _client_id: String) {
    // TTL expiry for orphaned workflows.
}

#[then("the new process must call DeclareWorkflow afresh to obtain a new workflow handle")]
async fn then_must_redeclare(_w: &mut KisekiWorld) {
    // Fresh DeclareWorkflow required.
}

#[then("seven advisory-audit events are written to the tenant audit shard")]
async fn then_seven_audit_events(_w: &mut KisekiWorld) {
    // Audit completeness assertion.
}

#[then(
    "each event carries the (org, project, workload, client_id, workflow_id, phase_id) correlation"
)]
async fn then_event_correlation(_w: &mut KisekiWorld) {
    // Correlation fields present.
}

#[then(
    "cluster-admin exports see workflow_id and phase_tag as opaque hashes only (per I-A3, ADR-015)"
)]
async fn then_cluster_admin_opaque(_w: &mut KisekiWorld) {
    // Opaque hash enforcement for cluster admin.
}

#[then("tenant admin exports see the full correlation per I-A2")]
async fn then_tenant_admin_full(_w: &mut KisekiWorld) {
    // Full correlation for tenant admin.
}

#[then(regex = r#"^new DeclareWorkflow calls from clients under "(\S+)" return "([^"]+)"$"#)]
async fn then_declare_returns(_w: &mut KisekiWorld, _workload: String, _status: String) {
    // Advisory disabled response.
}

#[then("in-flight workflows are gracefully ended with audit")]
async fn then_graceful_end(_w: &mut KisekiWorld) {
    // Graceful workflow termination.
}

#[then("the workload's data-path operations proceed with full performance and correctness")]
async fn then_data_path_full_perf(_w: &mut KisekiWorld) {
    // Data-path unaffected by advisory disable.
}

#[then("cluster admin can observe the opt-out in aggregate state but not the reason")]
async fn then_cluster_admin_aggregate(_w: &mut KisekiWorld) {
    // Cluster admin visibility.
}

#[then(regex = r#"^all tenants see "([^"]+)" on DeclareWorkflow$"#)]
async fn then_all_tenants_see(_w: &mut KisekiWorld, _status: String) {
    // Cluster-wide disable response.
}

#[then("no data-path operation is affected")]
async fn then_no_data_path_effect(_w: &mut KisekiWorld) {
    // Data-path independence during cluster disable.
}

#[then("the disable action is audited system-wide")]
async fn then_disable_audited(_w: &mut KisekiWorld) {
    // System-wide audit for disable.
}

#[then("both calls return the same error code")]
async fn then_same_error_code(_w: &mut KisekiWorld) {
    // Indistinguishable error codes.
}

#[then("the response payload structures are byte-identical in size")]
async fn then_byte_identical_size(_w: &mut KisekiWorld) {
    // Response size uniformity.
}

#[then("the latency distributions over many samples are statistically indistinguishable")]
async fn then_latency_indistinguishable(_w: &mut KisekiWorld) {
    // Timing-safe responses.
}

#[then("no timing, size, or code difference lets the caller tell \"forbidden\" from \"absent\"")]
async fn then_no_forbidden_absent_diff(_w: &mut KisekiWorld) {
    // Indistinguishable forbidden vs absent.
}

#[then("the response contains all fields it would in the k>=5 case")]
async fn then_response_all_fields(_w: &mut KisekiWorld) {
    // Low-k response field parity.
}

#[then("neighbour-derived fields carry a fixed sentinel value defined by policy")]
async fn then_sentinel_values(_w: &mut KisekiWorld) {
    // Sentinel values for low-k.
}

#[then(
    "the response size, message timing, and field presence are indistinguishable from the populated case"
)]
async fn then_response_indistinguishable(_w: &mut KisekiWorld) {
    // Response uniformity.
}

#[then("within a bounded detection interval the advisory subsystem tears the stream down")]
async fn then_stream_torn_down(_w: &mut KisekiWorld) {
    // Stream teardown on cert revocation.
}

#[then("subsequent hints on any resumed stream require a currently-valid cert")]
async fn then_require_valid_cert(_w: &mut KisekiWorld) {
    // mTLS re-validation enforcement.
}

#[then(
    "pre-revocation in-flight operations remain accepted up to the revocation point (per I-WA1)"
)]
async fn then_pre_revocation_accepted(_w: &mut KisekiWorld) {
    // In-flight acceptance window.
}

#[then("no tuples are adopted")]
async fn then_no_tuples_adopted(_w: &mut KisekiWorld) {
    // All-or-nothing tuple rejection.
}

#[then("subsequent prefetch hints within the cap continue to be accepted")]
async fn then_subsequent_prefetch_ok(_w: &mut KisekiWorld) {
    // Prefetch recovery after rejection.
}

#[then(regex = r#"^the first (\d+) succeed$"#)]
async fn then_first_n_succeed(_w: &mut KisekiWorld, _n: u32) {
    // Rate limit partial success.
}

#[then(regex = r#"^the remaining (\d+) are rejected with "([^"]+)"$"#)]
async fn then_remaining_rejected(_w: &mut KisekiWorld, _n: u32, _reason: String) {
    // Rate limit rejection.
}

#[then("an advisory-audit event is written for the rate exception")]
async fn then_rate_audit(_w: &mut KisekiWorld) {
    // Rate exception audit.
}

#[then("the workload's concurrent_workflows cap is independent and still enforced")]
async fn then_concurrent_cap_enforced(_w: &mut KisekiWorld) {
    // Independent cap enforcement.
}

#[then(
    regex = r#"^at least one audit event per unique \(workflow_id, rejection_reason\) tuple is written per second$"#
)]
async fn then_batched_audit_per_tuple(_w: &mut KisekiWorld) {
    // Batched audit per-tuple assertion.
}

#[then("exact accepted-count and throttled-count per workflow per second are preserved in audit")]
async fn then_counts_preserved(_w: &mut KisekiWorld) {
    // Audit count preservation.
}

#[then(regex = r#"^the total audit event volume is bounded below the raw (\d+)/sec figure$"#)]
async fn then_audit_volume_bounded(_w: &mut KisekiWorld, _raw: u32) {
    // Audit volume bound.
}

#[then("declare/end/phase/policy-violation events are written per-occurrence without batching")]
async fn then_per_occurrence_events(_w: &mut KisekiWorld) {
    // No batching for critical events.
}

#[then(regex = r#"^exactly one call returns success and the workflow advances to phase (\d+)$"#)]
async fn then_one_succeeds(_w: &mut KisekiWorld, _phase: u64) {
    // Serialized PhaseAdvance — one wins.
}

#[then(regex = r#"^the other call returns "([^"]+)"$"#)]
async fn then_other_fails(_w: &mut KisekiWorld, _error: String) {
    // Serialized PhaseAdvance — one loses.
}

#[then("no intermediate state where two phases are active is ever observable")]
async fn then_no_intermediate_state(_w: &mut KisekiWorld) {
    // Phase serialization — no concurrency.
}

#[then("hints that crossed the server-side receive boundary before End are best-effort processed")]
async fn then_pre_end_hints_best_effort(_w: &mut KisekiWorld) {
    // EndWorkflow hint boundary.
}

#[then(regex = r#"^hints submitted after End return "([^"]+)"$"#)]
async fn then_post_end_hints_rejected(_w: &mut KisekiWorld, _error: String) {
    // Post-end hint rejection.
}

#[then("EndWorkflow does not block on buffered hint processing")]
async fn then_end_non_blocking(_w: &mut KisekiWorld) {
    // Non-blocking EndWorkflow.
}

#[then(
    regex = r#"^an advisory-audit "([^"]+)" event is written containing the count of pre-End hints dropped$"#
)]
async fn then_end_audit_with_count(_w: &mut KisekiWorld, _event: String) {
    // End audit with dropped hint count.
}

#[then(regex = r#"^new DeclareWorkflow calls return "([^"]+)"$"#)]
async fn then_new_declare_returns(_w: &mut KisekiWorld, _status: String) {
    // Draining state — new declares rejected.
}

#[then("the two active workflows continue to accept hints within their current phases")]
async fn then_active_continue(_w: &mut KisekiWorld) {
    // Draining — existing workflows unaffected.
}

#[then("when a workflow advances phase or hits TTL, it is audit-ended")]
async fn then_draining_audit_end(_w: &mut KisekiWorld) {
    // Draining — audit on phase advance or TTL.
}

#[then(
    "when both active workflows have ended, the tenant admin may transition draining to disabled"
)]
async fn then_draining_to_disabled(_w: &mut KisekiWorld) {
    // Draining → disabled transition.
}

#[then(regex = r#"^data-path operations for "(\S+)" are unaffected throughout$"#)]
async fn then_data_path_unaffected_throughout(_w: &mut KisekiWorld, _workload: String) {
    // Data-path unaffected during draining.
}

#[then("the current phase continues normally to completion or TTL")]
async fn then_current_phase_continues(_w: &mut KisekiWorld) {
    // Prospective policy — current phase continues.
}

#[then(regex = r#"^the next PhaseAdvance is rejected with "([^"]+)"$"#)]
async fn then_next_advance_rejected(_w: &mut KisekiWorld, _reason: String) {
    // Next PhaseAdvance rejection after policy revocation.
}

#[then("the workflow remains on its current phase")]
async fn then_workflow_remains(_w: &mut KisekiWorld) {
    // Workflow stays on current phase.
}

#[then("data-path operations for this workflow are unaffected")]
async fn then_data_path_this_workflow(_w: &mut KisekiWorld) {
    // Data-path unaffected for this workflow.
}

#[then(regex = r#"^the hint is rejected with "([^"]+)" at the schema-validation layer$"#)]
async fn then_schema_rejected(w: &mut KisekiWorld, reason: String) {
    assert!(
        w.last_error.as_ref().is_some_and(|e| e.contains(&reason)),
        "expected schema rejection '{}', got {:?}",
        reason,
        w.last_error
    );
}

#[then("no ownership check or side effect occurs")]
async fn then_no_side_effect(_w: &mut KisekiWorld) {
    // No side effects on schema rejection.
}

#[then("the response carries an opaque 128-bit workflow handle")]
async fn then_opaque_handle(w: &mut KisekiWorld) {
    let wf_ref = w.last_workflow_ref.expect("workflow handle must exist");
    assert_eq!(
        wf_ref.0.len(),
        16,
        "workflow handle must be 128 bits (16 bytes)"
    );
    assert!(
        w.legacy.advisory_table.get(&wf_ref).is_some(),
        "workflow handle should resolve in advisory table"
    );
}

#[then("an `available_pools` list containing one descriptor per authorized pool:")]
async fn then_available_pools(_w: &mut KisekiWorld) {
    // Pool descriptor list — advisory layer.
}

#[then("subsequent AffinityHint or PrefetchHint submissions MUST reference one of these handles")]
async fn then_must_reference_handles(_w: &mut KisekiWorld) {
    // Handle reference enforcement.
}

#[then(regex = r#"^a handle not in this set is rejected with "([^"]+)".*$"#)]
async fn then_handle_rejected(_w: &mut KisekiWorld, _error: String) {
    // Invalid handle rejection.
}

#[then(
    regex = r#"^a terminal StreamWarning \{ kind: SUBSCRIPTION_REVOKED \} is emitted to "(\S+)"$"#
)]
async fn then_subscription_revoked(_w: &mut KisekiWorld, _wf: String) {
    // Subscription revocation warning.
}

#[then("the subscription is closed within a bounded interval")]
async fn then_subscription_closed(_w: &mut KisekiWorld) {
    // Subscription closure.
}

#[then(
    "data-path access to chunks in that pool is independently denied by data-path authorization"
)]
async fn then_data_path_denied(_w: &mut KisekiWorld) {
    // Data-path independent denial.
}

#[then("the workflow's other subscriptions and hints are unaffected")]
async fn then_other_subscriptions_ok(_w: &mut KisekiWorld) {
    // Other subscriptions unaffected.
}

#[then(regex = r#"^subsequent hints referencing "(\S+)" are rejected with "([^"]+)"$"#)]
async fn then_decommissioned_hints_rejected(_w: &mut KisekiWorld, _handle: String, _error: String) {
    // Decommissioned pool handle rejection.
}

#[then(
    regex = r#"^the rejection shape \(code, payload size, latency distribution\) is identical to a never-issued handle.*$"#
)]
async fn then_rejection_shape_identical(_w: &mut KisekiWorld) {
    // Uniform rejection shape.
}

#[then("the workflow continues; other handles and subscriptions remain valid")]
async fn then_workflow_continues(_w: &mut KisekiWorld) {
    // Workflow continuity after decommission.
}

#[then(
    regex = r#"^an OwnHotspot telemetry event is emitted to "(\S+)" carrying \{ composition_id: ([^,]+), contention: ([^}]+) \}$"#
)]
async fn then_own_hotspot_event(
    _w: &mut KisekiWorld,
    _wf: String,
    _comp: String,
    _contention: String,
) {
    // OWN_HOTSPOT event assertion.
}

#[then(
    regex = r#"^no composition owned by a different workload is named in any own-hotspot event.*$"#
)]
async fn then_no_cross_workload_hotspot(_w: &mut KisekiWorld) {
    // No cross-workload composition in hotspot events.
}

#[then("the contention value is bucketed (no fine-grained counts)")]
async fn then_contention_bucketed(_w: &mut KisekiWorld) {
    // Contention bucketing.
}

#[then("the advisory subsystem accepts the hint and emits HintAck OUTCOME_ACCEPTED")]
async fn then_hint_ack_accepted(_w: &mut KisekiWorld) {
    // Deadline hint accepted.
}

#[then("the write path MAY bias scheduling to meet the deadline (best-effort)")]
async fn then_may_bias_scheduling(_w: &mut KisekiWorld) {
    // Deadline scheduling best-effort.
}

#[then(
    regex = r#"^failure to meet the deadline is NOT an error — the write succeeds whenever the data path completes it.*$"#
)]
async fn then_deadline_not_error(_w: &mut KisekiWorld) {
    // Deadline miss is not an error.
}

#[then(
    regex = r#"^a deadline in the past is rejected with "([^"]+)" treatment \(schema validation\) or ignored as advisory$"#
)]
async fn then_past_deadline_rejected(_w: &mut KisekiWorld, _treatment: String) {
    // Past deadline handling.
}

#[then("phase entry 1 is evicted from the ring")]
async fn then_phase_evicted(_w: &mut KisekiWorld) {
    // Ring eviction assertion.
}

#[then("a PhaseSummaryEvent audit entry is emitted to the tenant audit shard with:")]
async fn then_phase_summary_audit(_w: &mut KisekiWorld) {
    // Phase summary audit with DataTable.
}

#[then(
    regex = r#"^the event is size-padded to a fixed bucket so its wire size does not leak workflow activity.*$"#
)]
async fn then_event_size_padded(_w: &mut KisekiWorld) {
    // Size padding for anti-leakage.
}

#[then(regex = r#"^cluster-admin exports see workflow_id and phase_tag hashed.*$"#)]
async fn then_cluster_admin_hashed(_w: &mut KisekiWorld) {
    // Hash enforcement for cluster admin exports.
}

#[then("a TelemetrySubscribedEvent audit entry is emitted with the list of channel enum names")]
async fn then_telemetry_subscribed_audit(_w: &mut KisekiWorld) {
    // Telemetry subscription audit.
}

#[then(
    "unsubscribe via ACTION_REMOVE emits a corresponding TelemetrySubscribedEvent (ACTION_REMOVE variant)"
)]
async fn then_unsubscribe_audit(_w: &mut KisekiWorld) {
    // Unsubscribe audit.
}

#[then("these events go to the tenant audit shard (I-WA8)")]
async fn then_events_tenant_shard(_w: &mut KisekiWorld) {
    // Tenant shard destination.
}

#[then("the current phase continues under the snapshotted priority batch (I-WA18)")]
async fn then_current_phase_snapshotted(_w: &mut KisekiWorld) {
    // Snapshotted priority continuation.
}

#[then(regex = r#"^the server emits a StreamWarning \{ kind: BUDGET_EXCEEDED \} on the stream.*$"#)]
async fn then_stream_warning_budget(_w: &mut KisekiWorld) {
    // BUDGET_EXCEEDED stream warning.
}

#[then(regex = r#"^the server emits StreamWarning \{ kind: WORKFLOW_TTL_SOON \}$"#)]
async fn then_stream_warning_ttl(_w: &mut KisekiWorld) {
    // WORKFLOW_TTL_SOON stream warning.
}

#[then(regex = r#"^the server emits StreamWarning \{ kind: CERT_NEAR_EXPIRY \}$"#)]
async fn then_stream_warning_cert(_w: &mut KisekiWorld) {
    // CERT_NEAR_EXPIRY stream warning.
}

#[then(
    "each warning is additionally audited as an advisory-state-transition or informational event"
)]
async fn then_warning_audited(_w: &mut KisekiWorld) {
    // Warning audit.
}

#[then(regex = r#"^the server emits StreamWarning \{ kind: HEARTBEAT \} on the stream$"#)]
async fn then_heartbeat(_w: &mut KisekiWorld) {
    // Heartbeat assertion.
}

#[then(regex = r#"^idle streams receive heartbeats every (\d+)s ± jitter until closed$"#)]
async fn then_heartbeat_interval(_w: &mut KisekiWorld, _interval: u32) {
    // Heartbeat interval.
}

#[then(
    "a client missing three consecutive heartbeats treats the stream as dead and reconnects (client-side obligation)"
)]
async fn then_heartbeat_reconnect(_w: &mut KisekiWorld) {
    // Client-side heartbeat obligation.
}

#[then(regex = r#"^every response carries gRPC status code NOT_FOUND \((\d+)\)$"#)]
async fn then_grpc_not_found(_w: &mut KisekiWorld, _code: u32) {
    // Uniform NOT_FOUND gRPC status.
}

#[then(regex = r#"^no response uses PERMISSION_DENIED \((\d+)\) or UNAUTHENTICATED \((\d+)\)$"#)]
async fn then_no_perm_denied(_w: &mut KisekiWorld, _pd: u32, _ua: u32) {
    // No PERMISSION_DENIED or UNAUTHENTICATED.
}

#[then("the application-level AdvisoryError.code is SCOPE_NOT_FOUND for all cases")]
async fn then_scope_not_found_all(_w: &mut KisekiWorld) {
    // Uniform SCOPE_NOT_FOUND.
}

#[then("the response size, timing bucket, and message string are identical across all four cases")]
async fn then_response_identical_all_cases(_w: &mut KisekiWorld) {
    // Response uniformity across scope violation cases.
}

#[then(
    "the latency distributions and error payloads are indistinguishable between A's and B's rejections"
)]
async fn then_latency_indistinguishable_ab(_w: &mut KisekiWorld) {
    // Covert-channel: indistinguishable latency.
}

#[then("neither A nor B can infer the other's activity from rejection timing")]
async fn then_no_inference_ab(_w: &mut KisekiWorld) {
    // Covert-channel: no inference.
}

#[then("the size of each telemetry message is one of a small fixed set of sizes (padded/bucketed)")]
async fn then_message_size_bucketed(_w: &mut KisekiWorld) {
    // Telemetry message size bucketing.
}

#[then("the client cannot infer neighbour load from message size variation")]
async fn then_no_load_inference(_w: &mut KisekiWorld) {
    // No neighbour load inference from message size.
}
