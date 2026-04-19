// Package advisory provides advisory policy management for the control plane.
//
// Profile allow-lists, hint budgets with inheritance, and opt-out state
// machine. Federation replicates policy but NOT workflow state (ADR-021 §6).
//
// Spec: I-WA7, I-WA18.
package advisory

import "fmt"

// HintBudget defines per-workload advisory rate limits.
type HintBudget struct {
	HintsPerSec        uint32
	MaxConcurrentFlows uint32
	PhasesPerWorkflow  uint32
	PrefetchBytesMax   uint64
}

// ProfilePolicy defines which workload profiles are allowed at a scope.
type ProfilePolicy struct {
	AllowedProfiles []string
}

// OptOutState tracks the advisory opt-out FSM for a scope.
type OptOutState string

const (
	// OptOutEnabled means advisory is active.
	OptOutEnabled OptOutState = "enabled"
	// OptOutDraining means advisory is being shut down gracefully.
	OptOutDraining OptOutState = "draining"
	// OptOutDisabled means advisory is fully disabled.
	OptOutDisabled OptOutState = "disabled"
)

// ScopePolicy is the advisory policy at a specific scope (org/project/workload).
type ScopePolicy struct {
	ScopeID  string
	ParentID string // empty for org-level
	Budget   HintBudget
	Profiles ProfilePolicy
	OptOut   OptOutState
}

// ValidateBudgetInheritance checks that a child's budget does not exceed
// the parent's ceiling (I-WA18: ChildExceedsParentCeiling).
func ValidateBudgetInheritance(parent, child HintBudget) error {
	if child.HintsPerSec > parent.HintsPerSec {
		return fmt.Errorf("hints/sec %d exceeds parent ceiling %d", child.HintsPerSec, parent.HintsPerSec)
	}
	if child.MaxConcurrentFlows > parent.MaxConcurrentFlows {
		return fmt.Errorf("concurrent flows %d exceeds parent ceiling %d", child.MaxConcurrentFlows, parent.MaxConcurrentFlows)
	}
	if child.PrefetchBytesMax > parent.PrefetchBytesMax {
		return fmt.Errorf("prefetch bytes %d exceeds parent ceiling %d", child.PrefetchBytesMax, parent.PrefetchBytesMax)
	}
	return nil
}

// ValidateProfileInheritance checks that a child's allowed profiles are
// a subset of the parent's (I-WA7: ProfileNotInParent).
func ValidateProfileInheritance(parent, child ProfilePolicy) error {
	parentSet := make(map[string]bool)
	for _, p := range parent.AllowedProfiles {
		parentSet[p] = true
	}
	for _, p := range child.AllowedProfiles {
		if !parentSet[p] {
			return fmt.Errorf("profile %q not in parent allow-list", p)
		}
	}
	return nil
}
