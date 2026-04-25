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

/// Cluster-wide advisory ceilings set by the cluster admin.
#[derive(Clone, Debug)]
pub struct ClusterAdvisoryCeilings {
    /// Maximum hints per second across the cluster.
    pub hints_per_sec: u32,
    /// Maximum concurrent workflows across the cluster.
    pub concurrent_workflows: u32,
    /// Maximum telemetry subscribers.
    pub telemetry_subscribers: u32,
    /// Maximum declared prefetch bytes.
    pub declared_prefetch_bytes: u64,
    /// Maximum workflow declares per second.
    pub workflow_declares_per_sec: u32,
}

/// Validate that an org-level budget does not exceed cluster ceilings.
pub fn validate_cluster_ceilings(
    ceilings: &ClusterAdvisoryCeilings,
    org_budget: &HintBudget,
) -> Result<(), ControlError> {
    if org_budget.hints_per_sec > ceilings.hints_per_sec {
        return Err(ControlError::Rejected("exceeds_cluster_ceiling".into()));
    }
    if org_budget.max_concurrent_flows > ceilings.concurrent_workflows {
        return Err(ControlError::Rejected("exceeds_cluster_ceiling".into()));
    }
    if org_budget.prefetch_bytes_max > ceilings.declared_prefetch_bytes {
        return Err(ControlError::Rejected("exceeds_cluster_ceiling".into()));
    }
    Ok(())
}

/// Compute the effective profile allow-list as the intersection of
/// org, project, and workload profiles.
#[must_use]
pub fn effective_profiles(
    org: &ProfilePolicy,
    project: &ProfilePolicy,
    workload: &ProfilePolicy,
) -> Vec<String> {
    workload
        .allowed_profiles
        .iter()
        .filter(|p| org.allowed_profiles.contains(p) && project.allowed_profiles.contains(p))
        .cloned()
        .collect()
}

/// Transition the opt-out state machine.
///
/// Valid transitions: Enabled -> Draining -> Disabled.
/// Cluster admin can also go Enabled -> Disabled directly.
pub fn transition_opt_out(
    current: &OptOutState,
    target: &OptOutState,
    is_cluster_admin: bool,
) -> Result<OptOutState, ControlError> {
    match (current, target) {
        (OptOutState::Enabled, OptOutState::Draining) => Ok(OptOutState::Draining),
        (OptOutState::Draining, OptOutState::Disabled) => Ok(OptOutState::Disabled),
        (OptOutState::Enabled, OptOutState::Disabled) if is_cluster_admin => {
            Ok(OptOutState::Disabled)
        }
        _ => Err(ControlError::Rejected(format!(
            "invalid opt-out transition: {current:?} -> {target:?}"
        ))),
    }
}

/// Check whether a profile is still allowed for an active workflow.
/// Returns Ok if the profile is in the allow-list, Err("profile_revoked")
/// if it has been removed (I-WA18: prospective application).
pub fn check_profile_for_phase_advance(
    current_profile: &str,
    current_allow_list: &ProfilePolicy,
) -> Result<(), ControlError> {
    if current_allow_list
        .allowed_profiles
        .contains(&current_profile.to_owned())
    {
        Ok(())
    } else {
        Err(ControlError::Rejected("profile_revoked".into()))
    }
}

/// Advisory audit event types.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum AdvisoryAuditEvent {
    /// A workflow was declared.
    DeclareWorkflow {
        workflow_id: String,
        org: String,
        project: String,
        workload: String,
    },
    /// A workflow ended.
    EndWorkflow { workflow_id: String, reason: String },
    /// A phase was advanced.
    PhaseAdvance {
        workflow_id: String,
        phase_id: String,
    },
    /// A policy violation rejection.
    PolicyViolation { workflow_id: String, reason: String },
    /// Budget was exceeded.
    BudgetExceeded { workflow_id: String, metric: String },
    /// Batched hint aggregates (I-WA8).
    HintAggregate { accepted: u64, throttled: u64 },
}

impl AdvisoryAuditEvent {
    /// For cluster-admin exports, workflow_id and phase_tag are opaque hashes (I-A3, I-WA8).
    #[must_use]
    pub fn as_cluster_admin_view(&self) -> (&'static str, String) {
        match self {
            Self::DeclareWorkflow { workflow_id, .. } => (
                "declare-workflow",
                format!("hash:{:x}", hash_opaque(workflow_id)),
            ),
            Self::EndWorkflow { workflow_id, .. } => (
                "end-workflow",
                format!("hash:{:x}", hash_opaque(workflow_id)),
            ),
            Self::PhaseAdvance { workflow_id, .. } => (
                "phase-advance",
                format!("hash:{:x}", hash_opaque(workflow_id)),
            ),
            Self::PolicyViolation { workflow_id, .. } => (
                "policy-violation",
                format!("hash:{:x}", hash_opaque(workflow_id)),
            ),
            Self::BudgetExceeded { workflow_id, .. } => (
                "budget-exceeded",
                format!("hash:{:x}", hash_opaque(workflow_id)),
            ),
            Self::HintAggregate { .. } => ("hint-aggregate", String::new()),
        }
    }
}

/// Simple hash for opaque workflow IDs in cluster-admin view.
fn hash_opaque(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(u64::from(b));
    }
    h
}

/// A pool authorization entry for a workload.
#[derive(Clone, Debug)]
pub struct PoolAuthorization {
    /// Tenant-chosen opaque label (exposed to caller).
    pub opaque_label: String,
    /// Cluster-internal pool ID (never exposed to caller).
    pub cluster_internal_pool: String,
}

/// Mint a pool handle — returns (handle, opaque_label). The cluster-internal
/// pool ID is never included in the response (I-WA11, I-WA19).
#[must_use]
pub fn mint_pool_handle(auth: &PoolAuthorization) -> (u128, String) {
    // Fresh 128-bit handle. In production this would use a CSPRNG;
    // for the guard we just derive from the label + a counter proxy.
    let handle = hash_opaque(&auth.opaque_label) as u128
        | ((hash_opaque(&auth.cluster_internal_pool) as u128) << 64);
    (handle, auth.opaque_label.clone())
    // Note: cluster_internal_pool is intentionally NOT returned.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_ceilings() -> ClusterAdvisoryCeilings {
        ClusterAdvisoryCeilings {
            hints_per_sec: 1000,
            concurrent_workflows: 64,
            telemetry_subscribers: 16,
            declared_prefetch_bytes: 256 * 1024 * 1024 * 1024, // 256GB
            workflow_declares_per_sec: 20,
        }
    }

    #[test]
    fn cluster_admin_sets_hint_budget_ceilings() {
        // Scenario: Cluster admin defines cluster-wide hint-budget ceilings
        let ceilings = cluster_ceilings();

        // Within ceiling — accepted
        let ok_budget = HintBudget {
            hints_per_sec: 500,
            max_concurrent_flows: 32,
            phases_per_workflow: 10,
            prefetch_bytes_max: 100 * 1024 * 1024 * 1024,
        };
        assert!(validate_cluster_ceilings(&ceilings, &ok_budget).is_ok());

        // Exceeds ceiling — rejected with "exceeds_cluster_ceiling"
        let bad_budget = HintBudget {
            hints_per_sec: 2000,
            max_concurrent_flows: 32,
            phases_per_workflow: 10,
            prefetch_bytes_max: 100 * 1024 * 1024 * 1024,
        };
        let err = validate_cluster_ceilings(&ceilings, &bad_budget).unwrap_err();
        assert!(err.to_string().contains("exceeds_cluster_ceiling"));
    }

    #[test]
    fn org_profile_allow_list_narrows_per_scope() {
        // Scenario: Org-level profile allow-list narrows per project and workload
        let org = ProfilePolicy {
            allowed_profiles: vec![
                "ai-training".into(),
                "ai-inference".into(),
                "hpc-checkpoint".into(),
                "batch-etl".into(),
            ],
        };
        let project = ProfilePolicy {
            allowed_profiles: vec!["ai-training".into(), "hpc-checkpoint".into()],
        };
        let workload = ProfilePolicy {
            allowed_profiles: vec!["ai-training".into()],
        };
        let effective = effective_profiles(&org, &project, &workload);
        assert_eq!(effective, vec!["ai-training"]);

        // Child cannot add a profile not in parent
        let bad_child = ProfilePolicy {
            allowed_profiles: vec!["ai-training".into(), "exotic-profile".into()],
        };
        let err = validate_profile_inheritance(&project, &bad_child).unwrap_err();
        assert!(err.to_string().contains("not in parent"));
    }

    #[test]
    fn workload_budget_cannot_exceed_project_ceiling() {
        // Scenario: Workload budget cannot exceed project ceiling
        let project_budget = HintBudget {
            hints_per_sec: 300,
            max_concurrent_flows: 10,
            phases_per_workflow: 5,
            prefetch_bytes_max: 1024 * 1024 * 1024,
        };
        let bad_workload = HintBudget {
            hints_per_sec: 500,
            max_concurrent_flows: 5,
            phases_per_workflow: 3,
            prefetch_bytes_max: 512 * 1024 * 1024,
        };
        let err = validate_budget_inheritance(&project_budget, &bad_workload).unwrap_err();
        assert!(err.to_string().contains("hints/sec"));
        assert!(err.to_string().contains("exceeds parent ceiling"));
    }

    #[test]
    fn tenant_admin_disables_advisory_three_state_transition() {
        // Scenario: Tenant admin disables Workflow Advisory - three-state transition
        let state = OptOutState::Enabled;

        // Enabled -> Draining
        let draining = transition_opt_out(&state, &OptOutState::Draining, false).unwrap();
        assert_eq!(draining, OptOutState::Draining);

        // Draining -> Disabled
        let disabled = transition_opt_out(&draining, &OptOutState::Disabled, false).unwrap();
        assert_eq!(disabled, OptOutState::Disabled);

        // Invalid: Enabled -> Disabled (non-cluster-admin)
        let err = transition_opt_out(&OptOutState::Enabled, &OptOutState::Disabled, false);
        assert!(err.is_err());

        // Invalid: Disabled -> Enabled
        let err = transition_opt_out(&OptOutState::Disabled, &OptOutState::Enabled, false);
        assert!(err.is_err());
    }

    #[test]
    fn cluster_admin_disables_advisory_cluster_wide() {
        // Scenario: Cluster admin disables Workflow Advisory cluster-wide during incident
        // Cluster admin can go directly Enabled -> Disabled
        let disabled =
            transition_opt_out(&OptOutState::Enabled, &OptOutState::Disabled, true).unwrap();
        assert_eq!(disabled, OptOutState::Disabled);
    }

    #[test]
    fn advisory_policy_changes_apply_prospectively() {
        // Scenario: Advisory policy changes apply prospectively to existing workflows
        // Active workflow continues under the policy effective at DeclareWorkflow.
        // If profile is removed from allow-list, next PhaseAdvance is rejected.
        let allow_list = ProfilePolicy {
            allowed_profiles: vec!["ai-inference".into()], // ai-training removed
        };
        // Current workflow uses ai-training — phase advance rejected
        let err = check_profile_for_phase_advance("ai-training", &allow_list).unwrap_err();
        assert_eq!(err.to_string(), "profile_revoked");

        // ai-inference is still allowed
        assert!(check_profile_for_phase_advance("ai-inference", &allow_list).is_ok());
    }

    #[test]
    fn tenant_audit_export_includes_advisory_events() {
        // Scenario: Tenant audit export includes advisory events
        let events: Vec<AdvisoryAuditEvent> = vec![
            AdvisoryAuditEvent::DeclareWorkflow {
                workflow_id: "wf-abc".into(),
                org: "org-pharma".into(),
                project: "clinical-trials".into(),
                workload: "training-run-42".into(),
            },
            AdvisoryAuditEvent::EndWorkflow {
                workflow_id: "wf-abc".into(),
                reason: "completed".into(),
            },
            AdvisoryAuditEvent::PhaseAdvance {
                workflow_id: "wf-abc".into(),
                phase_id: "compute".into(),
            },
            AdvisoryAuditEvent::PolicyViolation {
                workflow_id: "wf-abc".into(),
                reason: "profile_revoked".into(),
            },
            AdvisoryAuditEvent::BudgetExceeded {
                workflow_id: "wf-abc".into(),
                metric: "hints_per_sec".into(),
            },
            AdvisoryAuditEvent::HintAggregate {
                accepted: 1000,
                throttled: 50,
            },
        ];

        // Tenant admin sees full correlation
        if let AdvisoryAuditEvent::DeclareWorkflow {
            org,
            project,
            workload,
            workflow_id,
        } = &events[0]
        {
            assert_eq!(org, "org-pharma");
            assert_eq!(project, "clinical-trials");
            assert_eq!(workload, "training-run-42");
            assert_eq!(workflow_id, "wf-abc");
        } else {
            panic!("expected DeclareWorkflow");
        }

        // Cluster-admin view: workflow_id is opaque hash
        for event in &events {
            let (event_type, opaque_id) = event.as_cluster_admin_view();
            assert!(!event_type.is_empty());
            if !opaque_id.is_empty() {
                assert!(
                    opaque_id.starts_with("hash:"),
                    "cluster admin view should be hashed: {opaque_id}"
                );
                assert!(
                    !opaque_id.contains("wf-abc"),
                    "workflow_id must not appear in cluster admin view"
                );
            }
        }
    }

    #[test]
    fn workload_pool_authorization_produces_handles() {
        // Scenario: Workload pool authorization produces tenant-chosen labels
        let pools = vec![
            PoolAuthorization {
                opaque_label: "fast-nvme".into(),
                cluster_internal_pool: "pool-0af7".into(),
            },
            PoolAuthorization {
                opaque_label: "bulk-nvme".into(),
                cluster_internal_pool: "pool-921c".into(),
            },
        ];

        // First workflow
        let (handle1_a, label1_a) = mint_pool_handle(&pools[0]);
        let (handle1_b, label1_b) = mint_pool_handle(&pools[1]);

        // Labels match tenant-chosen names
        assert_eq!(label1_a, "fast-nvme");
        assert_eq!(label1_b, "bulk-nvme");

        // Handles are 128-bit (non-zero)
        assert_ne!(handle1_a, 0);
        assert_ne!(handle1_b, 0);

        // Different pools produce different handles
        assert_ne!(handle1_a, handle1_b);

        // The response (handle, label) never contains the cluster-internal pool ID.
        // This is verified structurally: mint_pool_handle returns (u128, String)
        // where the String is the opaque_label, not the cluster_internal_pool.
        let response_str = format!("{handle1_a} {label1_a}");
        assert!(
            !response_str.contains("pool-0af7"),
            "cluster-internal pool ID must never be in response"
        );
    }
}
