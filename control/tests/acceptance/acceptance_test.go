package acceptance

import (
	"os"
	"testing"

	"github.com/cucumber/godog"
)

func TestFeatures(t *testing.T) {
	suite := godog.TestSuite{
		ScenarioInitializer: InitializeScenario,
		Options: &godog.Options{
			Format:   "pretty",
			Paths:    []string{"../../../specs/features/control-plane.feature"},
			Output:   os.Stdout,
			Strict:   false, // Allow undefined steps (shows as pending)
			TestingT: t,
		},
	}

	if suite.Run() != 0 {
		t.Fatal("non-zero status returned, failed to run feature tests")
	}
}

func InitializeScenario(ctx *godog.ScenarioContext) {
	w := NewControlWorld()

	// Background
	ctx.Step(`^a Kiseki cluster managed by cluster admin "([^"]*)"$`, w.givenClusterAdmin)
	ctx.Step(`^tenant "([^"]*)" managed by tenant admin "([^"]*)"$`, w.givenTenantAdmin)

	// Tenant lifecycle
	ctx.Step(`^cluster admin "([^"]*)" receives a tenant creation request$`, w.givenCreationRequest)
	ctx.Step(`^the request is processed with:$`, w.whenRequestProcessed)
	ctx.Step(`^organization "([^"]*)" is created$`, w.thenOrgCreated)
	ctx.Step(`^a tenant admin role is provisioned$`, w.thenAdminProvisioned)
	ctx.Step(`^compliance tags \[([^\]]*)\] are set at org level$`, w.thenComplianceTags)
	ctx.Step(`^quotas are enforced from creation$`, w.thenQuotasEnforced)

	// Project
	ctx.Step(`^tenant admin "([^"]*)" for "([^"]*)"$`, w.givenTenantAdminFor)
	ctx.Step(`^they create project "([^"]*)":$`, w.whenCreateProject)
	ctx.Step(`^project "([^"]*)" is created under "([^"]*)"$`, w.thenProjectCreated)
	ctx.Step(`^it inherits org-level tags \[([^\]]*)\] plus its own \[([^\]]*)\]$`, w.thenInheritsTags)
	ctx.Step(`^effective compliance is \[([^\]]*)\]$`, w.thenEffectiveCompliance)
	ctx.Step(`^capacity quota (\d+)TB is carved from org's (\d+)TB$`, w.thenQuotaCarved)

	// Workload
	ctx.Step(`^tenant admin creates workload "([^"]*)" under "([^"]*)"$`, w.givenCreateWorkload)
	ctx.Step(`^the workload is configured with:$`, w.whenWorkloadConfigured)
	ctx.Step(`^workload "([^"]*)" is created$`, w.thenWorkloadCreated)
	ctx.Step(`^quotas are enforced within org ceiling$`, w.thenQuotasWithinCeiling)

	// IAM
	ctx.Step(`^cluster admin "([^"]*)" needs to diagnose an issue with "([^"]*)" data$`, w.givenAdminNeedsDiag)
	ctx.Step(`^"([^"]*)" submits an access request for "([^"]*)" config/logs$`, w.whenSubmitAccessRequest)
	ctx.Step(`^the request is queued for tenant admin "([^"]*)" approval$`, w.thenQueued)
	ctx.Step(`^"([^"]*)" cannot access tenant data until approved$`, w.thenCannotAccess)
	ctx.Step(`^"([^"]*)" approves "([^"]*)" access request$`, w.givenApproves)
	ctx.Step(`^the approval is processed with:$`, w.whenApprovalProcessed)
	ctx.Step(`^"([^"]*)" can read tenant config/logs for "([^"]*)" namespace only$`, w.thenCanRead)
	ctx.Step(`^access expires after (\d+) hours automatically$`, w.thenExpires)
	ctx.Step(`^"([^"]*)" denies "([^"]*)" access request$`, w.givenDenies)
	ctx.Step(`^"([^"]*)" cannot access any "([^"]*)" data$`, w.thenStillDenied)
}
