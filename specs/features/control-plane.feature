Feature: Control Plane - Tenancy, IAM, policy, placement, federation
  The Control Plane provides the declarative API for tenancy, IAM, policy,
  placement, discovery, compliance tagging, and federation. Manages both
  cluster-level (cluster admin) and tenant-level (tenant admin) configuration
  with zero-trust boundary between them.

  Background:
    Given a Kiseki cluster managed by cluster admin "admin-ops"
    And tenant "org-pharma" managed by tenant admin "pharma-admin"

  # --- Namespace management ---

  @integration
  Scenario: Create namespace triggers shard creation
    Given tenant admin creates namespace "patient-data" under "org-pharma"
    When the Control Plane processes the request
    Then a new shard is created for "patient-data"
    And compliance tags are inherited from the org/project
    And the namespace is associated with the tenant and shard
    And the shard is placed on nodes per affinity policy

  # --- Federation ---

  @integration
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

  @integration
  Scenario: Data residency enforcement in federation
    Given org "org-pharma" has namespace "swiss-patients" tagged [swiss-residency]
    And the residency policy requires data to stay in Switzerland
    When data replication to site-EU is attempted for "swiss-patients"
    Then the replication is blocked
    And only data without residency constraints replicates
    And the blocked replication attempt is recorded in the audit log

  @integration
  Scenario: Tenant config sync across federated sites
    Given org "org-pharma" exists at both site-EU and site-CH
    When tenant admin updates a quota at site-EU
    Then the config change replicates async to site-CH
    And site-CH enforces the new quota after sync

  # --- Failure paths ---

  @integration
  Scenario: Control plane unavailable - data path continues
    Given the Control Plane service is down
    Then existing data path continues (Log, Chunks, Views work with last-known config)
    And no new tenants can be created
    And no policy changes take effect
    And no placement decisions can be made for new shards
    And the cluster admin is alerted

  @integration
  Scenario: Quota enforcement during control plane outage
    Given the Control Plane is unavailable
    And quotas are cached locally by gateways and native clients
    When writes continue
    Then quotas are enforced using last-known cached values
    And actual usage may drift slightly from quota during outage
    And reconciliation occurs when Control Plane recovers

  # --- Workflow Advisory policy (ADR-020) ---

  @integration
  Scenario: Federation does NOT replicate advisory state
    Given "org-pharma" is federated across two sites with async config replication
    When a workflow is declared at site A
    Then the workflow handle and in-memory state are local to site A
    And no workflow_id is replicated to site B
    And profile allow-lists, hint budgets, and opt-out state (which are config) ARE replicated async
    And the advisory subsystem is independent per site

  # --- Client-side cache policy (ADR-031) ---

  @integration
  Scenario: Cache policy resolved during control plane outage
    Given the Control Plane is unavailable
    And a client connects to a storage node via data-path gRPC
    When the client requests cache policy
    Then the storage node returns last-known cached TenantConfig (stale tolerance)
    And the client operates within the last-known policy
