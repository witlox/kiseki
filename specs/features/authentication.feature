Feature: Authentication — mTLS, tenant identity, cluster admin IAM
  Data-fabric authentication via mTLS with per-tenant certificates
  signed by Cluster CA. Optional second-stage auth via tenant IdP.
  Cluster admin authenticates via Control Plane on management network.

  Background:
    Given a Kiseki cluster with Cluster CA "ca-root-001"
    And tenant "org-pharma" with certificate "cert-pharma-001" signed by "ca-root-001"
    And tenant "org-biotech" with certificate "cert-biotech-001" signed by "ca-root-001"

  # --- mTLS on data fabric (I-Auth1) ---

  Scenario: Valid tenant certificate — connection accepted
    Given a native client presents certificate "cert-pharma-001"
    When the storage node validates the certificate chain
    Then the certificate chain resolves to Cluster CA "ca-root-001"
    And the tenant_id is extracted from the certificate subject
    And the connection is accepted for tenant "org-pharma"

  Scenario: Invalid certificate — connection rejected
    Given a native client presents a self-signed certificate not signed by the Cluster CA
    When the storage node validates the certificate chain
    Then validation fails (not signed by Cluster CA)
    And the connection is rejected with TLS handshake error
    And the rejection is recorded in the audit log

  Scenario: Expired certificate — connection rejected
    Given tenant certificate "cert-pharma-001" has expired
    When the native client attempts to connect
    Then the connection is rejected with "certificate expired" error
    And the tenant admin is notified to renew

  Scenario: Revoked certificate — connection rejected
    Given tenant certificate "cert-pharma-001" has been revoked by the Cluster CA
    When the native client attempts to connect
    Then the storage node checks the certificate revocation list
    And the connection is rejected with "certificate revoked" error
    And the revocation attempt is recorded in the audit log

  Scenario: Certificate tenant mismatch — data access denied
    Given a native client presents valid certificate for "org-pharma"
    When it attempts to access data belonging to "org-biotech"
    Then the request is denied (tenant_id from cert != target tenant)
    And no data is returned
    And the attempt is recorded in the audit log

  # --- Optional second-stage auth (I-Auth2) ---

  Scenario: Tenant with IdP configured — second-stage validation
    Given "org-pharma" has configured an external IdP for workload identity
    And a native client presents valid mTLS cert for "org-pharma"
    When the client also presents a workload identity token from the IdP
    Then the token is validated against the tenant's IdP
    And the workload_id is extracted from the token
    And the connection is accepted with full workload identity (org + workload)

  Scenario: Tenant with IdP configured — missing token
    Given "org-pharma" has configured an external IdP (second stage required)
    And a native client presents valid mTLS cert but no workload token
    Then the connection is rejected with "workload identity required" error
    And the tenant admin is notified

  Scenario: Tenant without IdP — mTLS only (sufficient)
    Given "org-biotech" has NOT configured an external IdP
    And a native client presents valid mTLS cert for "org-biotech"
    Then the connection is accepted with org-level identity only
    And no second-stage auth is required

  # --- SPIFFE/SPIRE alternative (I-Auth3) ---

  Scenario: SPIFFE SVID presented instead of raw mTLS cert
    Given the cluster is configured to accept SPIFFE SVIDs
    And a native client presents a SPIFFE SVID with URI "spiffe://cluster/org/pharma/workload/training-42"
    When the storage node validates the SVID trust domain
    Then the tenant_id (org-pharma) and workload_id (training-42) are extracted
    And the connection is accepted

  # --- Cluster admin authentication (I-Auth4) ---

  Scenario: Cluster admin authenticates via control plane
    Given cluster admin "admin-ops" connects to the Control Plane API
    And the Control Plane is on the management network (not data fabric)
    When "admin-ops" authenticates with admin credentials
    Then access to cluster-level operations is granted
    And no access to tenant-scoped data is granted without approval (I-T4)

  Scenario: Cluster admin attempts data fabric access — rejected
    Given cluster admin "admin-ops" attempts to connect directly to a storage node
    And presents an admin credential (not a tenant certificate)
    Then the connection is rejected (admin creds not valid on data fabric)
    And admin must use the Control Plane API on the management network

  # --- Gateway authentication ---

  Scenario: NFS gateway authenticates incoming client
    Given an NFS client connects to gateway "gw-nfs-pharma"
    And the gateway is configured for tenant "org-pharma"
    When the NFS client authenticates (Kerberos, AUTH_SYS, or TLS)
    Then the gateway validates the client's identity against tenant config
    And maps the client identity to the tenant's authorization model
    And the NFS session is established

  Scenario: S3 gateway authenticates incoming request
    Given an S3 client sends a request with AWS SigV4 signature
    When the gateway "gw-s3-pharma" validates the signature
    Then the access key is resolved to a tenant + workload identity
    And the request is authorized against the tenant's policy

  # --- Workflow Advisory authorization (ADR-020) ---
  # The advisory channel uses the same mTLS tenant certificate as the
  # data path, and re-validates identity per operation — not just at
  # stream establishment (I-WA3).

  Scenario: mTLS identity re-validated per advisory operation
    Given a native client under workload "training-run-42" has an active bidi advisory stream
    And the stream was established using certificate "tenant-cert-v1"
    When the client submits a hint on the stream
    Then the advisory subsystem re-validates "tenant-cert-v1" for the owning workload before acting (I-WA3)
    And the hint is accepted if and only if the cert is currently valid for that workload

  Scenario: Advisory stream torn down on certificate revocation
    Given a workflow is active on a long-lived bidi advisory stream under cert "tenant-cert-v1"
    When the Cluster CA revokes "tenant-cert-v1" (e.g., rotation, compromise)
    Then within a bounded detection interval the advisory subsystem detects the revocation
    And tears the stream down with a clear error ("cert_revoked")
    And pre-revocation in-flight hints accepted before the detection point remain valid (they were advisory only, I-WA1)
    And the next advisory operation requires a fresh, valid cert

  Scenario: Workflow_id is a capability reference, mTLS is the authority
    Given workload "inference-svc-9" has somehow obtained a workflow_id belonging to "training-run-42"
    When "inference-svc-9" presents its own valid mTLS cert and the stolen workflow_id on the advisory channel
    Then the advisory subsystem rejects the operation with "workflow_not_found_in_scope" (I-WA3, I-WA10)
    And the error shape and latency distribution are identical to those for a never-issued workflow_id (I-WA6)
    And no information about "training-run-42"'s workflow state is revealed
