// Package acceptance provides BDD acceptance tests for the Kiseki control plane.
package acceptance

import (
	"github.com/witlox/kiseki/control/pkg/advisory"
	"github.com/witlox/kiseki/control/pkg/iam"
	"github.com/witlox/kiseki/control/pkg/tenant"
)

// ControlWorld holds shared state across all steps in a scenario.
// Reset for each scenario via InitializeScenario.
type ControlWorld struct {
	// Real implementations
	TenantStore *tenant.Store

	// Test state
	LastError       error
	LastOrgID       string
	LastProjectID   string
	LastWorkloadID  string
	LastAccessReq   *iam.AccessRequest
	LastBudgetError error
	AdvisoryBudget  *advisory.HintBudget
}

// NewControlWorld creates a fresh world for each scenario.
func NewControlWorld() *ControlWorld {
	return &ControlWorld{
		TenantStore: tenant.NewStore(),
		AdvisoryBudget: &advisory.HintBudget{
			HintsPerSec:        100,
			MaxConcurrentFlows: 10,
			PhasesPerWorkflow:  50,
			PrefetchBytesMax:   1024 * 1024,
		},
	}
}
