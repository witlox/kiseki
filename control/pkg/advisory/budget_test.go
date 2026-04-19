package advisory

import "testing"

func TestValidateBudgetInheritance(t *testing.T) {
	parent := HintBudget{HintsPerSec: 100, MaxConcurrentFlows: 10, PhasesPerWorkflow: 50, PrefetchBytesMax: 1024 * 1024}

	// Valid child.
	child := HintBudget{HintsPerSec: 50, MaxConcurrentFlows: 5, PhasesPerWorkflow: 25, PrefetchBytesMax: 512 * 1024}
	if err := ValidateBudgetInheritance(parent, child); err != nil {
		t.Errorf("expected valid, got: %v", err)
	}

	// Exceeds hints/sec.
	bad := HintBudget{HintsPerSec: 200, MaxConcurrentFlows: 5, PhasesPerWorkflow: 25, PrefetchBytesMax: 512 * 1024}
	if err := ValidateBudgetInheritance(parent, bad); err == nil {
		t.Error("expected error for hints/sec overflow")
	}
}

func TestValidateProfileInheritance(t *testing.T) {
	parent := ProfilePolicy{AllowedProfiles: []string{"ai-training", "hpc-checkpoint", "batch-etl"}}

	// Valid subset.
	child := ProfilePolicy{AllowedProfiles: []string{"ai-training", "batch-etl"}}
	if err := ValidateProfileInheritance(parent, child); err != nil {
		t.Errorf("expected valid, got: %v", err)
	}

	// Profile not in parent.
	bad := ProfilePolicy{AllowedProfiles: []string{"ai-inference"}}
	if err := ValidateProfileInheritance(parent, bad); err == nil {
		t.Error("expected error for profile not in parent")
	}
}
