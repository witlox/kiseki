Feature: Native Client — Client-side library with FUSE, encryption, and transport selection
  The Native Client runs in workload processes on compute nodes. Exposes
  POSIX (FUSE) and native API. Performs tenant-layer encryption — plaintext
  never leaves the workload process. Discovers shards/views/gateways
  dynamically from the data fabric without control plane access.

  Background:
    Given a compute node on the Slingshot fabric
    And tenant "org-pharma" with an active workload "training-run-42"
    And tenant KEK "pharma-kek-001" available via tenant KMS
    And native client library linked into the workload process

  # --- Bootstrap and discovery ---

  @library
  Scenario: Client bootstraps without control plane access
    Given the compute node is on the SAN fabric only (no control plane network)
    When the native client initializes
    Then it discovers available shards, views, and gateways via the data fabric
    And it authenticates with tenant credentials
    And it obtains tenant KEK material from the tenant KMS
    And it is ready to serve reads and writes
    And no direct control plane connectivity was required

  @library
  Scenario: Client selects best available transport
    Given the compute node has:
      | transport    | available |
      | libfabric/CXI| yes       |
      | RDMA verbs   | no        |
      | TCP          | yes       |
    When the native client initializes
    Then it selects libfabric/CXI as the primary transport (highest performance)
    And falls back to TCP if CXI connection fails
    And the transport selection is transparent to the workload

  # --- Native API read path ---

  # @unit scenarios moved to crate-level unit tests:
  # "Native API direct read" → kiseki-client/src/fuse_fs.rs::native_api_direct_read_same_path_as_fuse

  # --- Write path ---

  # --- RDMA path ---

  @library
  Scenario: One-sided RDMA read for pre-encrypted chunks
    Given the transport is libfabric/CXI with one-sided RDMA capability
    And chunk "c50" is stored as system-encrypted ciphertext on a storage node
    When the native client issues a one-sided RDMA read for "c50"
    Then the ciphertext is transferred directly to client memory (no target CPU)
    And the client decrypts in-process using tenant KEK → system DEK
    And the storage node CPU is not involved in the transfer
    And wire encryption is provided by the pre-encrypted nature of the chunk

  # --- Failure paths ---

  # @unit scenario "Native client process crashes" → kiseki-client/src/fuse_fs.rs::crash_semantics_committed_survives

  @library
  Scenario: Storage node unreachable — chunk read fails
    Given the native client requests chunk "c50" from a storage node
    And the storage node is unreachable
    Then the client attempts to read from an EC peer or replica
    And if an alternative source exists, the read succeeds
    And if no alternative exists, the read fails with EIO

  @library
  Scenario: Transport failover — CXI to TCP
    Given the native client is using libfabric/CXI
    When the CXI transport fails (NIC issue, fabric partition)
    Then the client falls back to TCP transport
    And operations continue at reduced performance
    And the client periodically attempts to reconnect via CXI
    And the failover is transparent to the workload

  # --- Discovery protocol (ADR-008) ---

  @library
  Scenario: All seed endpoints unreachable — discovery fails
    Given the native client is configured with seed list [node1:9100, node2:9100]
    And both seed endpoints are unreachable
    When the native client attempts to initialize
    Then discovery fails with retriable "no seeds reachable" error
    And the client retries with exponential backoff
    And the workload receives EIO until discovery succeeds

  @library
  Scenario: Discovery returns shard and view topology
    Given the native client connects to seed endpoint node1:9100
    When it sends a discovery request
    Then the response contains:
      | field              | example                                    |
      | shards             | [{shard_id, leader_node, key_range}, ...]   |
      | views              | [{view_id, protocol, endpoint}, ...]        |
      | gateways           | [{protocol, transport, endpoint}, ...]      |
      | auth_requirements  | mTLS required, IdP optional                 |
    And the client caches the discovery response with TTL
    And no tenant-sensitive information is in the discovery response

  # --- Edge cases ---

  @library
  Scenario: Multiple clients writing to the same file concurrently
    Given two native client instances on different compute nodes
    And both write to /mnt/kiseki/trials/shared-log.txt
    Then writes from both clients are serialized in the shard (Raft ordering)
    And the final state reflects a total order of all writes
    And neither client's writes are lost (though interleaving is possible)

  # @unit scenario "FUSE mount with read-only namespace" → kiseki-client/src/fuse_fs.rs::read_only_namespace_rejects_writes

  # --- Workflow Advisory integration (ADR-020) ---
  # @unit scenarios moved to crate-level unit tests:
  # "Client declares a workflow" → kiseki-client/src/advisory.rs::declare_workflow_returns_session_with_correlation
  # "Pattern-detector origin" → kiseki-client/src/advisory.rs::pattern_detector_emits_sequential_hint
  # "Client declares prefetch ranges" → kiseki-client/src/advisory.rs::prefetch_ranges_batched
  # "Client throttles on backpressure" → kiseki-client/src/advisory.rs::hard_backpressure_telemetry_has_retry_after
  # "Advisory disabled" → kiseki-client/src/advisory.rs::advisory_disabled_degrades_gracefully

  # =====================================================================
  # Client-side cache (ADR-031)
  # =====================================================================

  # @unit scenarios moved to crate-level unit tests:
  # "Pinned mode stages a dataset" → kiseki-client/src/staging.rs::pinned_mode_stages_dataset_with_manifest
  # "Staging handoff" → kiseki-client/src/staging.rs::staging_handoff_pool_adoption
  # "Staging beyond capacity" → kiseki-client/src/staging.rs::staging_beyond_capacity_no_eviction

  @library
  Scenario: Cache policy resolved via data-path gRPC
    Given a compute node with no gateway or control plane access
    And the client connects to a storage node via data-path gRPC
    When the client establishes a session
    Then cache policy is fetched via GetCachePolicy RPC on the data-path channel (I-CC9)
    And the client operates within the policy ceilings

  # @unit scenario "Per-node cache capacity enforcement" → kiseki-client/src/cache.rs::per_node_capacity_enforcement
