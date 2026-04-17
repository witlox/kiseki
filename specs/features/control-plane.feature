Feature: Control Plane — Tenancy, IAM, policy, placement, federation
  The Control Plane provides the declarative API for tenancy, IAM, policy,
  placement, discovery, compliance tagging, and federation. Manages both
  cluster-level (cluster admin) and tenant-level (tenant admin) configuration
  with zero-trust boundary between them.

  Background:
    Given a Kiseki cluster managed by cluster admin "admin-ops"
    And tenant "org-pharma" managed by tenant admin "pharma-admin"

  # --- Tenant lifecycle ---

  Scenario: Create a new organization (tenant)
    Given cluster admin "admin-ops" receives a tenant creation request
    When the request is processed with:
      | field             | value              |
      | org_name          | org-genomics       |
      | compliance_tags   | [HIPAA, GDPR]      |
      | quota_capacity    | 500TB              |
      | quota_iops        | 100000             |
      | dedup_policy      | cross-tenant (default) |
    Then organization "org-genomics" is created
    And a tenant admin role is provisioned
    And compliance tags [HIPAA, GDPR] are set at org level
    And quotas are enforced from creation
    And the tenant creation is recorded in the audit log

  Scenario: Create optional project within organization
    Given tenant admin "pharma-admin" for "org-pharma"
    When they create project "clinical-trials":
      | field             | value              |
      | quota_capacity    | 200TB              |
      | compliance_tags   | [revFADP]          |
    Then project "clinical-trials" is created under "org-pharma"
    And it inherits org-level tags [HIPAA, GDPR] plus its own [revFADP]
    And effective compliance is [HIPAA, GDPR, revFADP]
    And capacity quota 200TB is carved from org's 500TB

  Scenario: Create workload within tenant
    Given tenant admin creates workload "training-run-42" under "org-pharma"
    When the workload is configured with:
      | field             | value              |
      | quota_capacity    | 50TB               |
      | quota_iops        | 20000              |
    Then workload "training-run-42" is created
    And quotas are enforced within org ceiling
    And the workload can authenticate native clients and gateway access

  # --- Namespace management ---

  Scenario: Create namespace triggers shard creation
    Given tenant admin creates namespace "patient-data" under "org-pharma"
    When the Control Plane processes the request
    Then a new shard is created for "patient-data"
    And compliance tags are inherited from the org/project
    And the namespace is associated with the tenant and shard
    And the shard is placed on nodes per affinity policy

  # --- IAM and zero-trust boundary ---

  Scenario: Cluster admin requests access to tenant data — requires approval
    Given cluster admin "admin-ops" needs to diagnose an issue with "org-pharma" data
    When "admin-ops" submits an access request for "org-pharma" config/logs
    Then the request is queued for tenant admin "pharma-admin" approval
    And "admin-ops" cannot access tenant data until approved
    And the request and its outcome are recorded in the audit log

  Scenario: Cluster admin access request approved — scoped and time-limited
    Given "pharma-admin" approves "admin-ops" access request
    When the approval is processed with:
      | field         | value              |
      | scope         | namespace "trials" |
      | duration      | 4 hours            |
      | access_level  | read-only          |
    Then "admin-ops" can read tenant config/logs for "trials" namespace only
    And access expires after 4 hours automatically
    And all access during the window is recorded in the tenant audit export

  Scenario: Cluster admin access request denied
    Given "pharma-admin" denies "admin-ops" access request
    Then "admin-ops" cannot access any "org-pharma" tenant data
    And the denial is recorded in the audit log
    And "admin-ops" can only see cluster-level operational metrics (tenant-anonymous)

  Scenario: Tenant admin cannot access other tenant's data
    Given tenant admin "pharma-admin" for "org-pharma"
    When "pharma-admin" attempts to access "org-biotech" configuration
    Then the request is denied (full tenant isolation)
    And the attempt is recorded in the audit log

  # --- Quota enforcement ---

  Scenario: Write rejected when tenant quota exceeded
    Given "org-pharma" has used 499TB of 500TB capacity quota
    When a 2TB write is attempted
    Then the write is rejected with "quota exceeded" error
    And the rejection is reported to the protocol gateway / native client
    And the tenant admin is notified

  Scenario: Workload quota within org ceiling
    Given "org-pharma" has 500TB capacity, 300TB used
    And workload "training-run-42" has 50TB quota, 49TB used
    When a 2TB write is attempted by "training-run-42"
    Then the write is rejected (workload quota exceeded: 49 + 2 > 50)
    Even though org-level quota has headroom

  Scenario: Quota adjustment by tenant admin
    Given tenant admin increases workload "training-run-42" quota to 100TB
    When the adjustment is within org ceiling
    Then the new quota takes effect immediately
    And the change is recorded in the audit log

  # --- Placement and flavor management ---

  Scenario: Tenant selects a flavor — best-fit matching
    Given the cluster offers flavors:
      | flavor         | protocol | transport | topology        |
      | hpc-slingshot  | NFS      | CXI       | hyperconverged  |
      | standard-tcp   | S3       | TCP       | dedicated       |
      | ai-training    | NFS+S3   | CXI+TCP   | shared          |
    When "org-pharma" requests flavor "ai-training"
    And the cluster has CXI-capable nodes but not in "shared" topology
    Then the system provides best-fit: CXI transport, closest available topology
    And reports the actual configuration to the tenant admin
    And the mismatch is logged (requested vs. provided)

  Scenario: Flavor unavailable
    Given tenant requests flavor "quantum-rdma" which doesn't match any cluster capability
    Then the request is rejected with "no matching flavor available"
    And available flavors are listed in the response

  # --- Compliance tag management ---

  Scenario: Compliance tag inheritance — union of constraints
    Given org "org-pharma" has tags [HIPAA, GDPR]
    And project "clinical-trials" has tag [revFADP]
    And namespace "swiss-patients" has tag [swiss-residency]
    Then effective tags for "swiss-patients" are [HIPAA, GDPR, revFADP, swiss-residency]
    And the staleness floor is the strictest across all four regimes
    And data residency constraints from "swiss-residency" are enforced
    And audit requirements are the union of all regimes

  Scenario: Compliance tag cannot be removed if data exists under it
    Given namespace "trials" has tag [HIPAA] and contains compositions
    When tenant admin attempts to remove the HIPAA tag
    Then the removal is rejected
    And the reason: "cannot remove compliance tag with existing data; migrate or delete first"
    And the attempt is recorded in the audit log

  # --- Retention hold management ---

  Scenario: Set retention hold before crypto-shred
    Given tenant admin sets retention hold on namespace "trials":
      | field    | value                  |
      | hold_id  | hipaa-litigation-2026  |
      | scope    | namespace "trials"     |
      | ttl      | 7 years                |
    Then the hold is active on all chunks referenced by compositions in "trials"
    And physical GC is blocked for held chunks even if refcount drops to 0
    And the hold is recorded in the audit log

  Scenario: Release retention hold
    Given retention hold "hipaa-litigation-2026" has expired (or is released by tenant admin)
    When the hold is released
    Then chunks with refcount 0 become eligible for physical GC
    And the release is recorded in the audit log

  # --- Federation ---

  Scenario: Register federation peer
    Given cluster admin registers site-CH as a federation peer to site-EU
    When the peering is established:
      | field              | value                       |
      | peer_site          | site-CH                     |
      | replication_mode   | async                       |
      | tenant_config_sync | yes                         |
      | data_replication   | ciphertext only             |
    Then tenant config and discovery metadata replicate async between sites
    And data replication carries ciphertext (no key material)
    And both sites connect to the same tenant KMS per tenant

  Scenario: Data residency enforcement in federation
    Given org "org-pharma" has namespace "swiss-patients" tagged [swiss-residency]
    And the residency policy requires data to stay in Switzerland
    When data replication to site-EU is attempted for "swiss-patients"
    Then the replication is blocked
    And only data without residency constraints replicates
    And the blocked replication attempt is recorded in the audit log

  Scenario: Tenant config sync across federated sites
    Given org "org-pharma" exists at both site-EU and site-CH
    When tenant admin updates a quota at site-EU
    Then the config change replicates async to site-CH
    And site-CH enforces the new quota after sync

  # --- Maintenance mode ---

  Scenario: Cluster-wide maintenance mode
    Given cluster admin sets the cluster to maintenance mode
    Then all shards enter read-only mode
    And ShardMaintenanceEntered events are emitted
    And all write commands are rejected with retriable errors
    And reads continue from existing views
    And the maintenance window is recorded in the audit log

  # --- Failure paths ---

  Scenario: Control plane unavailable — data path continues
    Given the Control Plane service is down
    Then existing data path continues (Log, Chunks, Views work with last-known config)
    And no new tenants can be created
    And no policy changes take effect
    And no placement decisions can be made for new shards
    And the cluster admin is alerted

  Scenario: Quota enforcement during control plane outage
    Given the Control Plane is unavailable
    And quotas are cached locally by gateways and native clients
    When writes continue
    Then quotas are enforced using last-known cached values
    And actual usage may drift slightly from quota during outage
    And reconciliation occurs when Control Plane recovers

  # --- Workflow Advisory policy (ADR-020) ---
  # Control Plane owns profile allow-lists, hint budgets, and advisory
  # opt-out state. Policy inherits org → project → workload with each
  # level narrowing (never broadening) its parent. Data path is never
  # affected by policy changes here (I-WA2, I-WA18).

  Scenario: Cluster admin defines cluster-wide hint-budget ceilings
    Given cluster admin "admin-ops" sets cluster-wide Workflow Advisory ceilings:
      | field                   | value |
      | hints_per_sec           | 1000  |
      | concurrent_workflows    | 64    |
      | telemetry_subscribers   | 16    |
      | declared_prefetch_bytes | 256GB |
      | workflow_declares_per_sec | 20  |
    Then these values are enforced as upper bounds for all org-level settings
    And any attempt by a tenant admin to exceed them is rejected with "exceeds_cluster_ceiling"
    And the change is recorded in the cluster audit trail

  Scenario: Org-level profile allow-list narrows per project and workload
    Given tenant admin "pharma-admin" for "org-pharma" sets allowed profiles [ai-training, ai-inference, hpc-checkpoint, batch-etl]
    And project "clinical-trials" admin narrows allowed profiles to [ai-training, hpc-checkpoint]
    And workload "training-run-42" under "clinical-trials" declares allowed profiles [ai-training]
    Then the effective allowed profiles for "training-run-42" are the intersection = [ai-training]
    And a child scope cannot add a profile not present in its parent; such an attempt is rejected with "profile_not_in_parent"

  Scenario: Workload budget cannot exceed project ceiling
    Given project "clinical-trials" ceiling sets hints_per_sec 300
    When tenant admin attempts to set workload "training-run-42" hints_per_sec 500
    Then the update is rejected with "child_exceeds_parent_ceiling"
    And the workload's effective budget remains its last-valid value
    And the rejected change is audited

  Scenario: Tenant admin disables Workflow Advisory for a workload — three-state transition
    Given "training-run-42" has Workflow Advisory enabled with 2 active workflows
    When tenant admin transitions advisory state to "draining"
    Then new DeclareWorkflow calls from "training-run-42" clients return ADVISORY_DISABLED
    And the 2 active workflows continue accepting hints within their current phases
    And when each active workflow ends or TTLs, it is audit-ended
    When the tenant admin subsequently transitions draining → disabled
    Then all hint processing ends, active telemetry subscriptions close
    And data-path operations remain fully correct throughout (I-WA12)

  Scenario: Cluster admin disables Workflow Advisory cluster-wide during incident
    Given a suspected advisory-subsystem issue
    When cluster admin transitions cluster-wide state directly to "disabled"
    Then all tenants observe ADVISORY_DISABLED on new DeclareWorkflow calls
    And active workflows across tenants are audit-ended
    And no data-path operation is blocked, slowed, or fails (I-WA2)
    And the cluster-wide transition is recorded in the cluster audit trail

  Scenario: Advisory policy changes apply prospectively to existing workflows
    Given workflow "wf-abc" is active in phase "compute" under profile ai-training
    When tenant admin removes "ai-training" from the workload's allow-list
    Then "wf-abc" continues its current phase under the policy effective at DeclareWorkflow (I-WA18)
    And the next PhaseAdvance is rejected with "profile_revoked" and the workflow remains on its current phase
    And budget reductions take effect prospectively from the next second

  Scenario: Tenant audit export includes advisory events
    Given tenant admin "pharma-admin" retrieves the tenant audit export for the last 24h
    When the export is generated
    Then it includes advisory-audit events: declare-workflow, end-workflow, phase-advance, policy-violation rejections, budget-exceeded, and (batched per I-WA8) hint-accepted and hint-throttled aggregates
    And each event carries the (org, project, workload, client_id, workflow_id, phase_id, reason) correlation
    And cluster-admin exports over the same window see workflow_id and phase_tag as opaque hashes only (I-A3, I-WA8)

  Scenario: Federation does NOT replicate advisory state
    Given "org-pharma" is federated across two sites with async config replication
    When a workflow is declared at site A
    Then the workflow handle and in-memory state are local to site A
    And no workflow_id is replicated to site B
    And profile allow-lists, hint budgets, and opt-out state (which are config) ARE replicated async
    And the advisory subsystem is independent per site
