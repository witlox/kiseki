Feature: Authentication — mTLS, tenant identity, cluster admin IAM
  Data-fabric authentication via mTLS with per-tenant certificates
  signed by Cluster CA. Optional second-stage auth via tenant IdP.
  Cluster admin authenticates via Control Plane on management network.

  Background:
    Given a Kiseki cluster with Cluster CA "ca-root-001"
    And tenant "org-pharma" with certificate "cert-pharma-001" signed by "ca-root-001"
    And tenant "org-biotech" with certificate "cert-biotech-001" signed by "ca-root-001"

  # --- mTLS on data fabric (I-Auth1) ---
  # @unit scenarios moved to crate-level unit tests:
  #   - tcp_tls.rs: valid_tenant_cert_accepted, invalid_cert_not_trusted,
  #     expired_cert_rejected, tenant_mismatch_denied,
  #     cluster_admin_control_plane_accepted, cluster_admin_data_fabric_rejected
  #   - idp.rs: idp_configured_valid_token_accepted, idp_configured_missing_token_rejected,
  #     no_idp_config_mtls_only_sufficient
  #   - nfs_auth.rs: nfs_gateway_authenticates_client_to_tenant
  #   - s3_auth.rs: s3_gateway_resolves_access_key_to_tenant
  #   - advisory.rs: workflow_ref_is_opaque_128_bit, workflow_ref_hashable_for_table_lookup
