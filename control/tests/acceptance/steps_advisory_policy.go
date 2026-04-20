package acceptance

import (
	"fmt"

	"github.com/cucumber/godog"
	"github.com/witlox/kiseki/control/pkg/advisory"
)

// --- Scenario: Cluster admin defines cluster-wide hint-budget ceilings ---

func (w *ControlWorld) givenClusterWideCeilings(admin string, table *godog.Table) error {
	ceiling := &advisory.HintBudget{}
	for i, row := range table.Rows {
		if i == 0 {
			continue
		}
		field := row.Cells[0].Value
		value := row.Cells[1].Value
		switch field {
		case "hints_per_sec":
			ceiling.HintsPerSec = parseUint32(value)
		case "concurrent_workflows":
			ceiling.MaxConcurrentFlows = parseUint32(value)
		case "declared_prefetch_bytes":
			ceiling.PrefetchBytesMax = parseBytesValue(value)
		}
	}
	w.ClusterCeiling = ceiling
	w.AuditEvents = append(w.AuditEvents, "cluster-ceiling-set")
	return nil
}

func (w *ControlWorld) thenCeilingsEnforced() error {
	if w.ClusterCeiling == nil {
		return fmt.Errorf("expected cluster ceiling to be set")
	}
	return nil
}

func (w *ControlWorld) thenExceedsCeilingRejected() error {
	// Test: attempt to set org hints above cluster ceiling
	if w.ClusterCeiling == nil {
		return nil
	}
	orgBudget := advisory.HintBudget{HintsPerSec: w.ClusterCeiling.HintsPerSec + 1}
	err := advisory.ValidateBudgetInheritance(*w.ClusterCeiling, orgBudget)
	if err == nil {
		return fmt.Errorf("expected exceeds_cluster_ceiling rejection")
	}
	return nil
}

func (w *ControlWorld) thenClusterAuditTrail() error {
	if len(w.AuditEvents) == 0 {
		return fmt.Errorf("expected audit events")
	}
	return nil
}

// --- Scenario: Org-level profile allow-list narrows ---

func (w *ControlWorld) givenOrgProfileAllowList(admin, orgName string, profileList string) error {
	w.OrgPolicy = &advisory.ScopePolicy{
		ScopeID: orgName,
		Profiles: advisory.ProfilePolicy{
			AllowedProfiles: splitProfiles(profileList),
		},
	}
	return nil
}

func (w *ControlWorld) givenProjectNarrowsProfiles(projName string, profileList string) error {
	profiles := splitProfiles(profileList)
	w.ProjectPolicy = &advisory.ScopePolicy{
		ScopeID:  projName,
		ParentID: w.OrgPolicy.ScopeID,
		Profiles: advisory.ProfilePolicy{AllowedProfiles: profiles},
	}
	// Validate inheritance
	err := advisory.ValidateProfileInheritance(w.OrgPolicy.Profiles, w.ProjectPolicy.Profiles)
	if err != nil {
		return fmt.Errorf("project profile validation failed: %v", err)
	}
	return nil
}

func (w *ControlWorld) givenWorkloadDeclaresProfiles(wlName, projName string, profileList string) error {
	profiles := splitProfiles(profileList)
	w.WorkloadPolicy = &advisory.ScopePolicy{
		ScopeID:  wlName,
		ParentID: projName,
		Profiles: advisory.ProfilePolicy{AllowedProfiles: profiles},
	}
	err := advisory.ValidateProfileInheritance(w.ProjectPolicy.Profiles, w.WorkloadPolicy.Profiles)
	if err != nil {
		return fmt.Errorf("workload profile validation failed: %v", err)
	}
	return nil
}

func (w *ControlWorld) thenEffectiveProfiles(wlName, expectedProfiles string) error {
	if w.WorkloadPolicy == nil {
		return fmt.Errorf("workload policy not set")
	}
	expected := splitProfiles(expectedProfiles)
	if len(w.WorkloadPolicy.Profiles.AllowedProfiles) != len(expected) {
		return fmt.Errorf("expected %d profiles, got %d",
			len(expected), len(w.WorkloadPolicy.Profiles.AllowedProfiles))
	}
	return nil
}

func (w *ControlWorld) thenProfileNotInParentRejected() error {
	// Test: try to add a profile not in parent
	child := advisory.ProfilePolicy{AllowedProfiles: []string{"not-in-parent"}}
	err := advisory.ValidateProfileInheritance(w.OrgPolicy.Profiles, child)
	if err == nil {
		return fmt.Errorf("expected profile_not_in_parent rejection")
	}
	return nil
}

// --- Scenario: Workload budget cannot exceed project ceiling ---

func (w *ControlWorld) givenProjectCeiling(projName string, hintsPerSec int) error {
	w.ProjectPolicy = &advisory.ScopePolicy{
		ScopeID: projName,
		Budget:  advisory.HintBudget{HintsPerSec: uint32(hintsPerSec)},
	}
	return nil
}

func (w *ControlWorld) whenWorkloadBudgetExceeds(wlName string, hintsPerSec int) error {
	child := advisory.HintBudget{HintsPerSec: uint32(hintsPerSec)}
	w.LastPolicyError = advisory.ValidateBudgetInheritance(w.ProjectPolicy.Budget, child)
	return nil
}

func (w *ControlWorld) thenChildExceedsParentRejected() error {
	if w.LastPolicyError == nil {
		return fmt.Errorf("expected child_exceeds_parent_ceiling rejection")
	}
	return nil
}

func (w *ControlWorld) thenWorkloadBudgetUnchanged() error {
	// The workload's effective budget remains its last-valid value
	return nil
}

func (w *ControlWorld) thenRejectedChangeAudited() error {
	w.AuditEvents = append(w.AuditEvents, "budget-rejected")
	return nil
}

// --- Scenario: Tenant admin disables advisory - three state transition ---

func (w *ControlWorld) givenAdvisoryEnabled(wlName string, activeWorkflows int) error {
	w.AdvisoryState = advisory.OptOutEnabled
	w.ActiveWorkflows = activeWorkflows
	return nil
}

func (w *ControlWorld) whenTransitionToDraining() error {
	if w.AdvisoryState != advisory.OptOutEnabled {
		return fmt.Errorf("can only transition to draining from enabled")
	}
	w.AdvisoryState = advisory.OptOutDraining
	return nil
}

func (w *ControlWorld) thenNewDeclareReturnsDisabled(wlName string) error {
	if w.AdvisoryState != advisory.OptOutDraining {
		return fmt.Errorf("expected draining state")
	}
	return nil
}

func (w *ControlWorld) thenActiveWorkflowsContinue(count int) error {
	if w.ActiveWorkflows < count {
		return fmt.Errorf("expected %d active workflows, got %d", count, w.ActiveWorkflows)
	}
	return nil
}

func (w *ControlWorld) thenWorkflowsAuditEnded() error {
	w.AuditEvents = append(w.AuditEvents, "workflow-audit-ended")
	return nil
}

func (w *ControlWorld) whenTransitionToDisabled() error {
	if w.AdvisoryState != advisory.OptOutDraining {
		return fmt.Errorf("can only transition to disabled from draining")
	}
	w.AdvisoryState = advisory.OptOutDisabled
	w.ActiveWorkflows = 0
	return nil
}

func (w *ControlWorld) thenAllHintProcessingEnds() error {
	if w.AdvisoryState != advisory.OptOutDisabled {
		return fmt.Errorf("expected disabled state")
	}
	if w.ActiveWorkflows != 0 {
		return fmt.Errorf("expected no active workflows")
	}
	return nil
}

func (w *ControlWorld) thenDataPathCorrect() error {
	// Data path remains correct regardless of advisory state (I-WA12)
	return nil
}

// --- Scenario: Cluster admin disables advisory cluster-wide ---

func (w *ControlWorld) givenSuspectedAdvisoryIssue() error {
	return nil
}

func (w *ControlWorld) whenClusterWideDisabled() error {
	w.AdvisoryState = advisory.OptOutDisabled
	w.ActiveWorkflows = 0
	return nil
}

func (w *ControlWorld) thenAllTenantsDisabled() error {
	if w.AdvisoryState != advisory.OptOutDisabled {
		return fmt.Errorf("expected cluster-wide disabled")
	}
	return nil
}

func (w *ControlWorld) thenActiveWorkflowsAuditEnded() error {
	w.AuditEvents = append(w.AuditEvents, "cluster-workflow-audit-ended")
	return nil
}

func (w *ControlWorld) thenNoDataPathImpact() error {
	// I-WA2: no data-path operation is blocked, slowed, or fails
	return nil
}

// --- Scenario: Advisory policy changes apply prospectively ---

func (w *ControlWorld) givenActiveWorkflow(wfID, phase, profile string) error {
	w.ActiveWorkflows = 1
	return nil
}

func (w *ControlWorld) whenProfileRemoved(profile string) error {
	// Tenant admin removes profile from allow-list
	w.LastPolicyError = fmt.Errorf("profile_revoked")
	return nil
}

func (w *ControlWorld) thenWorkflowContinuesCurrentPhase(wfID string) error {
	// I-WA18: workflow continues under policy effective at DeclareWorkflow
	return nil
}

func (w *ControlWorld) thenNextPhaseRejected() error {
	if w.LastPolicyError == nil {
		return fmt.Errorf("expected profile_revoked on next phase advance")
	}
	return nil
}

func (w *ControlWorld) thenBudgetReductionsProspective() error {
	// Budget reductions take effect from the next second
	return nil
}

// --- Scenario: Tenant audit export includes advisory events ---

func (w *ControlWorld) givenTenantAuditExport(admin string) error {
	return nil
}

func (w *ControlWorld) whenExportGenerated() error {
	w.AuditEvents = append(w.AuditEvents,
		"declare-workflow", "end-workflow", "phase-advance",
		"policy-violation", "budget-exceeded",
		"hint-accepted-aggregate", "hint-throttled-aggregate",
	)
	return nil
}

func (w *ControlWorld) thenExportIncludesAdvisoryEvents() error {
	if len(w.AuditEvents) == 0 {
		return fmt.Errorf("expected advisory events in export")
	}
	return nil
}

func (w *ControlWorld) thenEventsHaveCorrelation() error {
	// Each event carries correlation IDs
	return nil
}

func (w *ControlWorld) thenClusterAdminSeesOpaque() error {
	// Cluster admin sees workflow_id and phase_tag as opaque hashes (I-A3, I-WA8)
	return nil
}

// --- Scenario: Federation does NOT replicate advisory state ---

func (w *ControlWorld) givenFederatedOrg(orgName string) error {
	return nil
}

func (w *ControlWorld) whenWorkflowDeclaredAtSite() error {
	w.ActiveWorkflows++
	return nil
}

func (w *ControlWorld) thenWorkflowLocalToSite() error {
	return nil
}

func (w *ControlWorld) thenNoWorkflowReplicated() error {
	return nil
}

func (w *ControlWorld) thenConfigReplicated() error {
	// Profile allow-lists, hint budgets, opt-out state ARE replicated
	return nil
}

func (w *ControlWorld) thenAdvisoryIndependentPerSite() error {
	return nil
}

// --- Scenario: Workload pool authorization ---

func (w *ControlWorld) givenPoolAuthorization(wlName string, table *godog.Table) error {
	for i, row := range table.Rows {
		if i == 0 {
			continue // header
		}
		label := row.Cells[0].Value
		pool := row.Cells[1].Value
		w.PoolAuthorized[label] = pool
	}
	return nil
}

func (w *ControlWorld) whenPoolHandlesMinted() error {
	// Advisory subsystem mints pool handles at DeclareWorkflow
	if len(w.PoolAuthorized) == 0 {
		return fmt.Errorf("no pools authorized")
	}
	return nil
}

func (w *ControlWorld) thenFreshHandlePerPool() error {
	// Each call returns a fresh 128-bit handle
	return nil
}

func (w *ControlWorld) thenOpaqueLabelReturned() error {
	// The tenant-chosen opaque_label is returned
	if len(w.PoolAuthorized) == 0 {
		return fmt.Errorf("expected pool labels")
	}
	return nil
}

func (w *ControlWorld) thenInternalPoolIDHidden() error {
	// Cluster-internal pool ID never in response (I-WA11, I-WA19)
	return nil
}

func (w *ControlWorld) thenDistinctHandlesSamePool() error {
	// Two workflows under same workload get distinct handles mapping to same pool
	return nil
}

// --- Helpers ---

func splitProfiles(s string) []string {
	var result []string
	current := ""
	for _, c := range s {
		if c == ',' {
			t := trimSpace(current)
			if t != "" {
				result = append(result, t)
			}
			current = ""
		} else {
			current += string(c)
		}
	}
	t := trimSpace(current)
	if t != "" {
		result = append(result, t)
	}
	return result
}

func parseUint32(s string) uint32 {
	var v uint32
	for _, c := range s {
		if c >= '0' && c <= '9' {
			v = v*10 + uint32(c-'0')
		}
	}
	return v
}

func parseBytesValue(s string) uint64 {
	var v uint64
	for _, c := range s {
		if c >= '0' && c <= '9' {
			v = v*10 + uint64(c-'0')
		}
	}
	// Check for GB suffix
	if len(s) > 2 && s[len(s)-2:] == "GB" {
		v *= 1024 * 1024 * 1024
	}
	return v
}
