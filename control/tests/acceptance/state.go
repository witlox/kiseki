// Package acceptance provides BDD acceptance tests for the Kiseki control plane.
package acceptance

import (
	"github.com/witlox/kiseki/control/pkg/advisory"
	"github.com/witlox/kiseki/control/pkg/federation"
	"github.com/witlox/kiseki/control/pkg/flavor"
	"github.com/witlox/kiseki/control/pkg/iam"
	"github.com/witlox/kiseki/control/pkg/maintenance"
	"github.com/witlox/kiseki/control/pkg/namespace"
	"github.com/witlox/kiseki/control/pkg/retention"
	"github.com/witlox/kiseki/control/pkg/tenant"
)

// ControlWorld holds shared state across all steps in a scenario.
// Reset for each scenario via InitializeScenario.
type ControlWorld struct {
	// Real implementations
	TenantStore    *tenant.Store
	NamespaceStore *namespace.Store
	RetentionStore *retention.Store
	FederationReg  *federation.Registry
	Maintenance    *maintenance.State

	// Flavor state
	FlavorList      []flavor.Flavor
	LastFlavorMatch *flavor.Flavor
	LastFlavorError error

	// Advisory state
	AdvisoryBudget   *advisory.HintBudget
	AdvisoryEnabled  bool
	ClusterCeiling   *advisory.HintBudget
	OrgPolicy        *advisory.ScopePolicy
	ProjectPolicy    *advisory.ScopePolicy
	WorkloadPolicy   *advisory.ScopePolicy
	AdvisoryState    advisory.OptOutState
	ActiveWorkflows  int
	LastPolicyError  error
	LastBudgetError  error
	ControlPlaneUp   bool
	AuditEvents      []string
	PoolAuthorized   map[string]string // opaque_label -> internal pool

	// Test state
	LastError      error
	LastOrgID      string
	LastProjectID  string
	LastWorkloadID string
	LastAccessReq  *iam.AccessRequest

	// Quota test state
	OrgCapacityUsed     uint64
	OrgCapacityTotal    uint64
	WorkloadCapUsed     uint64
	WorkloadCapTotal    uint64
	LastWriteError      error
	LastQuotaAdjustment bool
}

// NewControlWorld creates a fresh world for each scenario.
func NewControlWorld() *ControlWorld {
	return &ControlWorld{
		TenantStore:    tenant.NewStore(),
		NamespaceStore: namespace.NewStore(),
		RetentionStore: retention.NewStore(),
		FederationReg:  federation.NewRegistry(),
		Maintenance:    maintenance.NewState(),
		FlavorList:     flavor.DefaultFlavors(),
		AdvisoryBudget: &advisory.HintBudget{
			HintsPerSec:        100,
			MaxConcurrentFlows: 10,
			PhasesPerWorkflow:  50,
			PrefetchBytesMax:   1024 * 1024,
		},
		AdvisoryEnabled: true,
		AdvisoryState:   advisory.OptOutEnabled,
		ControlPlaneUp:  true,
		AuditEvents:     make([]string, 0),
		PoolAuthorized:  make(map[string]string),
	}
}
