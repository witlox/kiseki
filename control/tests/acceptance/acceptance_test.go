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

	// === Background ===
	ctx.Step(`^a Kiseki cluster managed by cluster admin "([^"]*)"$`, w.givenClusterAdmin)
	ctx.Step(`^tenant "([^"]*)" managed by tenant admin "([^"]*)"$`, w.givenTenantAdmin)

	// === Tenant lifecycle ===
	ctx.Step(`^cluster admin "([^"]*)" receives a tenant creation request$`, w.givenCreationRequest)
	ctx.Step(`^the request is processed with:$`, w.whenRequestProcessed)
	ctx.Step(`^organization "([^"]*)" is created$`, w.thenOrgCreated)
	ctx.Step(`^a tenant admin role is provisioned$`, w.thenAdminProvisioned)
	ctx.Step(`^compliance tags \[([^\]]*)\] are set at org level$`, w.thenComplianceTags)
	ctx.Step(`^quotas are enforced from creation$`, w.thenQuotasEnforced)
	ctx.Step(`^the tenant creation is recorded in the audit log$`, w.thenAuditRecorded)

	// === Project ===
	ctx.Step(`^tenant admin "([^"]*)" for "([^"]*)"$`, w.givenTenantAdminFor)
	ctx.Step(`^they create project "([^"]*)":$`, w.whenCreateProject)
	ctx.Step(`^project "([^"]*)" is created under "([^"]*)"$`, w.thenProjectCreated)
	ctx.Step(`^it inherits org-level tags \[([^\]]*)\] plus its own \[([^\]]*)\]$`, w.thenInheritsTags)
	ctx.Step(`^effective compliance is \[([^\]]*)\]$`, w.thenEffectiveCompliance)
	ctx.Step(`^capacity quota (\d+)TB is carved from org's (\d+)TB$`, w.thenQuotaCarved)

	// === Workload ===
	ctx.Step(`^tenant admin creates workload "([^"]*)" under "([^"]*)"$`, w.givenCreateWorkload)
	ctx.Step(`^the workload is configured with:$`, w.whenWorkloadConfigured)
	ctx.Step(`^workload "([^"]*)" is created$`, w.thenWorkloadCreated)
	ctx.Step(`^quotas are enforced within org ceiling$`, w.thenQuotasWithinCeiling)
	ctx.Step(`^the workload can authenticate native clients and gateway access$`, w.thenWorkloadCanAuth)

	// === Namespace ===
	ctx.Step(`^tenant admin creates namespace "([^"]*)" under "([^"]*)"$`, w.givenCreateNamespace)
	ctx.Step(`^the Control Plane processes the request$`, w.whenControlPlaneProcesses)
	ctx.Step(`^a new shard is created for "([^"]*)"$`, w.thenShardCreated)
	ctx.Step(`^compliance tags are inherited from the org/project$`, w.thenComplianceTagsInherited)
	ctx.Step(`^the namespace is associated with the tenant and shard$`, w.thenNamespaceAssociated)
	ctx.Step(`^the shard is placed on nodes per affinity policy$`, w.thenShardPlaced)

	// === IAM ===
	ctx.Step(`^cluster admin "([^"]*)" needs to diagnose an issue with "([^"]*)" data$`, w.givenAdminNeedsDiag)
	ctx.Step(`^"([^"]*)" submits an access request for "([^"]*)" config/logs$`, w.whenSubmitAccessRequest)
	ctx.Step(`^the request is queued for tenant admin "([^"]*)" approval$`, w.thenQueued)
	ctx.Step(`^"([^"]*)" cannot access tenant data until approved$`, w.thenCannotAccess)
	ctx.Step(`^the request and its outcome are recorded in the audit log$`, w.thenAuditRecorded)
	ctx.Step(`^"([^"]*)" approves "([^"]*)" access request$`, w.givenApproves)
	ctx.Step(`^the approval is processed with:$`, w.whenApprovalProcessed)
	ctx.Step(`^"([^"]*)" can read tenant config/logs for "([^"]*)" namespace only$`, w.thenCanRead)
	ctx.Step(`^access expires after (\d+) hours automatically$`, w.thenExpires)
	ctx.Step(`^all access during the window is recorded in the tenant audit export$`, w.thenAuditRecorded)
	ctx.Step(`^"([^"]*)" denies "([^"]*)" access request$`, w.givenDenies)
	ctx.Step(`^"([^"]*)" cannot access any "([^"]*)" (?:tenant )?data$`, w.thenStillDenied)
	ctx.Step(`^the denial is recorded in the audit log$`, w.thenAuditRecorded)
	ctx.Step(`^"([^"]*)" can only see cluster-level operational metrics \(tenant-anonymous\)$`, w.thenClusterMetricsOnly)

	// === Tenant isolation ===
	ctx.Step(`^"([^"]*)" attempts to access "([^"]*)" configuration$`, w.whenAccessAttempt)
	ctx.Step(`^the request is denied \(full tenant isolation\)$`, w.thenTenantIsolation)
	ctx.Step(`^the attempt is recorded in the audit log$`, w.thenAuditRecorded)

	// === Quota enforcement ===
	ctx.Step(`^"([^"]*)" has used (\d+)TB of (\d+)TB capacity quota$`, w.givenOrgCapacityUsed)
	ctx.Step(`^a (\d+)TB write is attempted$`, w.whenWriteAttempted)
	ctx.Step(`^the write is rejected with "quota exceeded" error$`, w.thenWriteRejectedQuota)
	ctx.Step(`^the rejection is reported to the protocol gateway / native client$`, w.thenRejectionReported)
	ctx.Step(`^the tenant admin is notified$`, w.thenTenantAdminNotified)
	ctx.Step(`^"([^"]*)" has (\d+)TB capacity, (\d+)TB used$`, w.givenOrgCapacityWithHeadroom)
	ctx.Step(`^workload "([^"]*)" has (\d+)TB quota, (\d+)TB used$`, w.givenWorkloadCapacity)
	ctx.Step(`^a (\d+)TB write is attempted by "([^"]*)"$`, w.whenWorkloadWriteAttempted)
	ctx.Step(`^the write is rejected \(workload quota exceeded: (\d+) \+ (\d+) > (\d+)\)$`, w.thenWorkloadWriteRejectedMsg)
	ctx.Step(`^org-level quota still has headroom$`, w.thenOrgHasHeadroom)
	ctx.Step(`^tenant admin increases workload "([^"]*)" quota to (\d+)TB$`, w.givenQuotaAdjustment)
	ctx.Step(`^the adjustment is within org ceiling$`, w.whenAdjustmentWithinCeiling)
	ctx.Step(`^the new quota takes effect immediately$`, w.thenNewQuotaTakesEffect)
	ctx.Step(`^the change is recorded in the audit log$`, w.thenAuditRecorded)

	// === Flavor / Placement ===
	ctx.Step(`^the cluster offers flavors:$`, w.givenClusterOffersFlavors)
	ctx.Step(`^"([^"]*)" requests flavor "([^"]*)"$`, w.whenRequestsFlavor)
	ctx.Step(`^the cluster has CXI-capable nodes but not in "([^"]*)" topology$`, w.whenClusterHasCapability)
	ctx.Step(`^the system provides best-fit: CXI transport, closest available topology$`, w.thenBestFitProvided)
	ctx.Step(`^reports the actual configuration to the tenant admin$`, w.thenActualConfigReported)
	ctx.Step(`^the mismatch is logged \(requested vs\. provided\)$`, w.thenMismatchLogged)
	ctx.Step(`^tenant requests flavor "([^"]*)" which doesn't match any cluster capability$`, w.givenFlavorUnavailable)
	ctx.Step(`^the request is rejected with "no matching flavor available"$`, w.thenFlavorRejected)
	ctx.Step(`^available flavors are listed in the response$`, w.thenAvailableFlavorsListed)

	// === Compliance tags ===
	ctx.Step(`^org "([^"]*)" has tags \[([^\]]*)\]$`, w.givenOrgHasTags)
	ctx.Step(`^project "([^"]*)" has tag \[([^\]]*)\]$`, w.givenProjectHasTag)
	ctx.Step(`^namespace "([^"]*)" has tag \[([^\]]*)\]$`, w.givenNamespaceHasTag)
	ctx.Step(`^effective tags for "([^"]*)" are \[([^\]]*)\]$`, w.thenEffectiveTagsAre)
	ctx.Step(`^the staleness floor is the strictest across all four regimes$`, w.thenStalenessFloorStrictest)
	ctx.Step(`^data residency constraints from "([^"]*)" are enforced$`, w.thenDataResidencyEnforcedTag)
	ctx.Step(`^audit requirements are the union of all regimes$`, w.thenAuditRequirementsUnion)
	ctx.Step(`^namespace "([^"]*)" has tag \[([^\]]*)\] and contains compositions$`, w.givenNamespaceHasTagAndData)
	ctx.Step(`^tenant admin attempts to remove the HIPAA tag$`, w.whenRemoveComplianceTag)
	ctx.Step(`^the removal is rejected$`, w.thenRemovalRejected)
	ctx.Step(`^the reason: "([^"]*)"$`, w.thenRemovalReason)

	// === Retention holds ===
	ctx.Step(`^tenant admin sets retention hold on namespace "([^"]*)":$`, w.givenRetentionHoldSetNs)
	ctx.Step(`^the hold is active on all chunks referenced by compositions in "([^"]*)"$`, w.thenHoldActive)
	ctx.Step(`^physical GC is blocked for held chunks even if refcount drops to 0$`, w.thenGCBlocked)
	ctx.Step(`^the hold is recorded in the audit log$`, w.thenAuditRecorded)
	ctx.Step(`^retention hold "([^"]*)" has expired \(or is released by tenant admin\)$`, w.givenHoldExpired)
	ctx.Step(`^the hold is released$`, w.whenHoldReleased)
	ctx.Step(`^chunks with refcount 0 become eligible for physical GC$`, w.thenChunksEligibleForGC)
	ctx.Step(`^the release is recorded in the audit log$`, w.thenAuditRecorded)

	// === Federation ===
	ctx.Step(`^cluster admin registers ([^ ]*) as a federation peer to ([^ ]*)$`, w.givenRegisterPeer)
	ctx.Step(`^the peering is established:$`, w.whenPeeringEstablished)
	ctx.Step(`^tenant config and discovery metadata replicate async between sites$`, w.thenConfigReplicatesAsync)
	ctx.Step(`^data replication carries ciphertext \(no key material\)$`, w.thenDataCiphertextOnly)
	ctx.Step(`^both sites connect to the same tenant KMS per tenant$`, w.thenSameKMS)
	ctx.Step(`^org "([^"]*)" has namespace "([^"]*)" tagged \[([^\]]*)\]$`, w.givenResidencyNamespace)
	ctx.Step(`^the residency policy requires data to stay in Switzerland$`, w.givenResidencyPolicy)
	ctx.Step(`^data replication to ([^ ]*) is attempted for "([^"]*)"$`, w.whenReplicationAttemptedSite)
	ctx.Step(`^the replication is blocked$`, w.thenReplicationBlocked)
	ctx.Step(`^only data without residency constraints replicates$`, w.thenOnlyUnconstrainedReplicates)
	ctx.Step(`^the blocked replication attempt is recorded in the audit log$`, w.thenAuditRecorded)
	ctx.Step(`^org "([^"]*)" exists at both ([^ ]*) and ([^ ]*)$`, w.givenOrgExistsBothSites)
	ctx.Step(`^tenant admin updates a quota at ([^ ]*)$`, w.whenQuotaUpdatedAtSite)
	ctx.Step(`^the config change replicates async to ([^ ]*)$`, w.thenConfigReplicatesToSite)
	ctx.Step(`^([^ ]*) enforces the new quota after sync$`, w.thenSiteEnforcesNewQuota)

	// === Maintenance mode ===
	ctx.Step(`^cluster admin sets the cluster to maintenance mode$`, w.givenMaintenanceMode)
	ctx.Step(`^all shards enter read-only mode$`, w.thenShardsReadOnly)
	ctx.Step(`^ShardMaintenanceEntered events are emitted$`, w.thenMaintenanceEventsEmitted)
	ctx.Step(`^all write commands are rejected with retriable errors$`, w.thenWritesRejectedRetriable)
	ctx.Step(`^reads continue from existing views$`, w.thenReadsFromViews)
	ctx.Step(`^the maintenance window is recorded in the audit log$`, w.thenAuditRecorded)

	// === Control plane unavailable ===
	ctx.Step(`^the Control Plane service is down$`, w.givenControlPlaneDown)
	ctx.Step(`^existing data path continues \(Log, Chunks, Views work with last-known config\)$`, w.thenDataPathContinues)
	ctx.Step(`^no new tenants can be created$`, w.thenNoNewTenants)
	ctx.Step(`^no policy changes take effect$`, w.thenNoPolicyChanges)
	ctx.Step(`^no placement decisions can be made for new shards$`, w.thenNoPlacementDecisions)
	ctx.Step(`^the cluster admin is alerted$`, w.thenClusterAdminAlerted)

	// === Quota during outage ===
	ctx.Step(`^the Control Plane is unavailable$`, w.givenControlPlaneUnavailable)
	ctx.Step(`^quotas are cached locally by gateways and native clients$`, w.givenQuotasCachedLocally)
	ctx.Step(`^writes continue$`, w.whenWritesContinue)
	ctx.Step(`^quotas are enforced using last-known cached values$`, w.thenCachedQuotasEnforced)
	ctx.Step(`^actual usage may drift slightly from quota during outage$`, w.thenUsageMayDrift)
	ctx.Step(`^reconciliation occurs when Control Plane recovers$`, w.thenReconciliationOnRecovery)

	// === Workflow Advisory: cluster ceilings ===
	ctx.Step(`^cluster admin "([^"]*)" sets cluster-wide Workflow Advisory ceilings:$`, w.givenClusterWideCeilings)
	ctx.Step(`^these values are enforced as upper bounds for all org-level settings$`, w.thenCeilingsEnforced)
	ctx.Step(`^any attempt by a tenant admin to exceed them is rejected with "exceeds_cluster_ceiling"$`, w.thenExceedsCeilingRejected)
	ctx.Step(`^the change is recorded in the cluster audit trail$`, w.thenClusterAuditTrail)

	// === Workflow Advisory: profile narrowing ===
	ctx.Step(`^tenant admin "([^"]*)" for "([^"]*)" sets allowed profiles \[([^\]]*)\]$`, w.givenOrgProfileAllowList)
	ctx.Step(`^project "([^"]*)" admin narrows allowed profiles to \[([^\]]*)\]$`, w.givenProjectNarrowsProfiles)
	ctx.Step(`^workload "([^"]*)" under "([^"]*)" declares allowed profiles \[([^\]]*)\]$`, w.givenWorkloadDeclaresProfiles)
	ctx.Step(`^the effective allowed profiles for "([^"]*)" are the intersection = \[([^\]]*)\]$`, w.thenEffectiveProfiles)
	ctx.Step(`^a child scope cannot add a profile not present in its parent; such an attempt is rejected with "profile_not_in_parent"$`, w.thenProfileNotInParentRejected)

	// === Workflow Advisory: budget ceiling ===
	ctx.Step(`^project "([^"]*)" ceiling sets hints_per_sec (\d+)$`, w.givenProjectCeiling)
	ctx.Step(`^tenant admin attempts to set workload "([^"]*)" hints_per_sec (\d+)$`, w.whenWorkloadBudgetExceeds)
	ctx.Step(`^the update is rejected with "child_exceeds_parent_ceiling"$`, w.thenChildExceedsParentRejected)
	ctx.Step(`^the workload's effective budget remains its last-valid value$`, w.thenWorkloadBudgetUnchanged)
	ctx.Step(`^the rejected change is audited$`, w.thenRejectedChangeAudited)

	// === Workflow Advisory: three-state transition ===
	ctx.Step(`^"([^"]*)" has Workflow Advisory enabled with (\d+) active workflows$`, w.givenAdvisoryEnabled)
	ctx.Step(`^tenant admin transitions advisory state to "draining"$`, w.whenTransitionToDraining)
	ctx.Step(`^new DeclareWorkflow calls from "([^"]*)" clients return ADVISORY_DISABLED$`, w.thenNewDeclareReturnsDisabled)
	ctx.Step(`^the (\d+) active workflows continue accepting hints within their current phases$`, w.thenActiveWorkflowsContinue)
	ctx.Step(`^when each active workflow ends or TTLs, it is audit-ended$`, w.thenWorkflowsAuditEnded)
	ctx.Step(`^the tenant admin subsequently transitions draining -> disabled$`, w.whenTransitionToDisabled)
	ctx.Step(`^all hint processing ends, active telemetry subscriptions close$`, w.thenAllHintProcessingEnds)
	ctx.Step(`^data-path operations remain fully correct throughout \(I-WA12\)$`, w.thenDataPathCorrect)

	// === Workflow Advisory: cluster-wide disable ===
	ctx.Step(`^a suspected advisory-subsystem issue$`, w.givenSuspectedAdvisoryIssue)
	ctx.Step(`^cluster admin transitions cluster-wide state directly to "disabled"$`, w.whenClusterWideDisabled)
	ctx.Step(`^all tenants observe ADVISORY_DISABLED on new DeclareWorkflow calls$`, w.thenAllTenantsDisabled)
	ctx.Step(`^active workflows across tenants are audit-ended$`, w.thenActiveWorkflowsAuditEnded)
	ctx.Step(`^no data-path operation is blocked, slowed, or fails \(I-WA2\)$`, w.thenNoDataPathImpact)
	ctx.Step(`^the cluster-wide transition is recorded in the cluster audit trail$`, w.thenClusterAuditTrail)

	// === Workflow Advisory: prospective policy changes ===
	ctx.Step(`^workflow "([^"]*)" is active in phase "([^"]*)" under profile ([^ ]*)$`, w.givenActiveWorkflow)
	ctx.Step(`^tenant admin removes "([^"]*)" from the workload's allow-list$`, w.whenProfileRemoved)
	ctx.Step(`^"([^"]*)" continues its current phase under the policy effective at DeclareWorkflow \(I-WA18\)$`, w.thenWorkflowContinuesCurrentPhase)
	ctx.Step(`^the next PhaseAdvance is rejected with "profile_revoked" and the workflow remains on its current phase$`, w.thenNextPhaseRejected)
	ctx.Step(`^budget reductions take effect prospectively from the next second$`, w.thenBudgetReductionsProspective)

	// === Workflow Advisory: audit export ===
	ctx.Step(`^tenant admin "([^"]*)" retrieves the tenant audit export for the last 24h$`, w.givenTenantAuditExport)
	ctx.Step(`^the export is generated$`, w.whenExportGenerated)
	ctx.Step(`^it includes advisory-audit events: declare-workflow, end-workflow, phase-advance, policy-violation rejections, budget-exceeded, and \(batched per I-WA8\) hint-accepted and hint-throttled aggregates$`, w.thenExportIncludesAdvisoryEvents)
	ctx.Step(`^each event carries the \(org, project, workload, client_id, workflow_id, phase_id, reason\) correlation$`, w.thenEventsHaveCorrelation)
	ctx.Step(`^cluster-admin exports over the same window see workflow_id and phase_tag as opaque hashes only \(I-A3, I-WA8\)$`, w.thenClusterAdminSeesOpaque)

	// === Workflow Advisory: federation ===
	ctx.Step(`^"([^"]*)" is federated across two sites with async config replication$`, w.givenFederatedOrg)
	ctx.Step(`^a workflow is declared at site A$`, w.whenWorkflowDeclaredAtSite)
	ctx.Step(`^the workflow handle and in-memory state are local to site A$`, w.thenWorkflowLocalToSite)
	ctx.Step(`^no workflow_id is replicated to site B$`, w.thenNoWorkflowReplicated)
	ctx.Step(`^profile allow-lists, hint budgets, and opt-out state \(which are config\) ARE replicated async$`, w.thenConfigReplicated)
	ctx.Step(`^the advisory subsystem is independent per site$`, w.thenAdvisoryIndependentPerSite)

	// === Workflow Advisory: pool authorization ===
	ctx.Step(`^tenant admin authorises workload "([^"]*)" for pools with labels:$`, w.givenPoolAuthorization)
	ctx.Step(`^the advisory subsystem mints pool handles at a DeclareWorkflow call$`, w.whenPoolHandlesMinted)
	ctx.Step(`^each call returns a fresh 128-bit handle per authorised pool$`, w.thenFreshHandlePerPool)
	ctx.Step(`^the tenant-chosen .opaque_label. is returned alongside each handle$`, w.thenOpaqueLabelReturned)
	ctx.Step(`^the cluster-internal pool ID is never included in any response to the caller \(I-WA11, I-WA19\)$`, w.thenInternalPoolIDHidden)
	ctx.Step(`^two workflows under the same workload receive distinct handles mapping to the same internal pool$`, w.thenDistinctHandlesSamePool)
}
