Feature: Authentication — mTLS, tenant identity, cluster admin IAM
  Data-fabric authentication via mTLS with per-tenant certificates
  signed by Cluster CA. Optional second-stage auth via tenant IdP.
  Cluster admin authenticates via Control Plane on management network.

  Background:
    Given a Kiseki cluster with Cluster CA "ca-root-001"
    And tenant "org-pharma" with certificate "cert-pharma-001" signed by "ca-root-001"
    And tenant "org-biotech" with certificate "cert-biotech-001" signed by "ca-root-001"

  # --- mTLS on data fabric (I-Auth1) ---

  @unit
  Scenario: Valid tenant certificate — connection accepted
    Given a native client presents certificate "cert-pharma-001"
    When the storage node validates the certificate chain
    Then the certificate chain resolves to Cluster CA "ca-root-001"
    And the tenant_id is extracted from the certificate subject
    And the connection is accepted for tenant "org-pharma"

  @unit
  Scenario: Invalid certificate — connection rejected
    Given a native client presents a self-signed certificate not signed by the Cluster CA
    When the storage node validates the certificate chain
    Then validation fails (not signed by Cluster CA)
    And the connection is rejected with TLS handshake error
    And the rejection is recorded in the audit log

  @unit
  Scenario: Expired certificate — connection rejected
    Given tenant certificate "cert-pharma-001" has expired
    When the native client attempts to connect
    Then the connection is rejected with "certificate expired" error
    And the tenant admin is notified to renew

  @unit
  Scenario: Certificate tenant mismatch — data access denied
    Given a native client presents valid certificate for "org-pharma"
    When it attempts to access data belonging to "org-biotech"
    Then the request is denied (tenant_id from cert != target tenant)
    And no data is returned
    And the attempt is recorded in the audit log

  # --- Optional second-stage auth (I-Auth2) ---

  @unit
  Scenario: Tenant with IdP configured — second-stage validation
    Given "org-pharma" has configured an external IdP for workload identity
    And a native client presents valid mTLS cert for "org-pharma"
    When the client also presents a workload identity token from the IdP
    Then the token is validated against the tenant's IdP
    And the workload_id is extracted from the token
    And the connection is accepted with full workload identity (org + workload)

  @unit
  Scenario: Tenant with IdP configured — missing token
    Given "org-pharma" has configured an external IdP (second stage required)
    And a native client presents valid mTLS cert but no workload token
    Then the connection is rejected with "workload identity required" error
    And the tenant admin is notified

  @unit
  Scenario: Tenant without IdP — mTLS only (sufficient)
    Given "org-biotech" has NOT configured an external IdP
    And a native client presents valid mTLS cert for "org-biotech"
    Then the connection is accepted with org-level identity only
    And no second-stage auth is required

  # --- Cluster admin authentication (I-Auth4) ---

  @unit
  Scenario: Cluster admin authenticates via control plane
    Given cluster admin "admin-ops" connects to the Control Plane API
    And the Control Plane is on the management network (not data fabric)
    When "admin-ops" authenticates with admin credentials
    Then access to cluster-level operations is granted
    And no access to tenant-scoped data is granted without approval (I-T4)

  @unit
  Scenario: Cluster admin attempts data fabric access — rejected
    Given cluster admin "admin-ops" attempts to connect directly to a storage node
    And presents an admin credential (not a tenant certificate)
    Then the connection is rejected (admin creds not valid on data fabric)
    And admin must use the Control Plane API on the management network

  # --- Gateway authentication ---

  @unit
  Scenario: NFS gateway authenticates incoming client
    Given an NFS client connects to gateway "gw-nfs-pharma"
    And the gateway is configured for tenant "org-pharma"
    When the NFS client authenticates (Kerberos, AUTH_SYS, or TLS)
    Then the gateway validates the client's identity against tenant config
    And maps the client identity to the tenant's authorization model
    And the NFS session is established

  @unit
  Scenario: S3 gateway authenticates incoming request
    Given an S3 client sends a request with AWS SigV4 signature
    When the gateway "gw-s3-pharma" validates the signature
    Then the access key is resolved to a tenant + workload identity
    And the request is authorized against the tenant's policy

  # --- Workflow Advisory authorization (ADR-020) ---
  # The advisory channel uses the same mTLS tenant certificate as the
  # data path, and re-validates identity per operation — not just at
  # stream establishment (I-WA3).

  @unit
  Scenario: Workflow_id is a capability reference, mTLS is the authority
    Given workload "inference-svc-9" has somehow obtained a workflow_id belonging to "training-run-42"
    When "inference-svc-9" presents its own valid mTLS cert and the stolen workflow_id on the advisory channel
    Then the advisory subsystem rejects the operation with "workflow_not_found_in_scope" (I-WA3, I-WA10)
    And the error shape and latency distribution are identical to those for a never-issued workflow_id (I-WA6)
    And no information about "training-run-42"'s workflow state is revealed
