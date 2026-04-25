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

  @integration
  Scenario: Client bootstraps without control plane access
    Given the compute node is on the SAN fabric only (no control plane network)
    When the native client initializes
    Then it discovers available shards, views, and gateways via the data fabric
    And it authenticates with tenant credentials
    And it obtains tenant KEK material from the tenant KMS
    And it is ready to serve reads and writes
    And no direct control plane connectivity was required

  @integration
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

  @unit
  Scenario: Native API direct read — bypass FUSE overhead
    Given the workload uses the native Rust API directly
    When it calls kiseki_read(namespace, path, offset, length)
    Then the read path is the same as FUSE but without FUSE kernel overhead
    And latency is lower for small reads
    And the API returns a buffer with plaintext data

  # --- Write path ---

  # --- RDMA path ---

  @integration
  Scenario: One-sided RDMA read for pre-encrypted chunks
    Given the transport is libfabric/CXI with one-sided RDMA capability
    And chunk "c50" is stored as system-encrypted ciphertext on a storage node
    When the native client issues a one-sided RDMA read for "c50"
    Then the ciphertext is transferred directly to client memory (no target CPU)
    And the client decrypts in-process using tenant KEK → system DEK
    And the storage node CPU is not involved in the transfer
    And wire encryption is provided by the pre-encrypted nature of the chunk

  # --- Failure paths ---

  @unit
  Scenario: Native client process crashes — uncommitted writes lost
    Given the workload process crashes
    Then all in-flight uncommitted writes are lost
    And committed writes (acknowledged) are durable in the Log
    And other clients and views are unaffected
    And no cluster-wide impact

  @integration
  Scenario: Storage node unreachable — chunk read fails
    Given the native client requests chunk "c50" from a storage node
    And the storage node is unreachable
    Then the client attempts to read from an EC peer or replica
    And if an alternative source exists, the read succeeds
    And if no alternative exists, the read fails with EIO

  @integration
  Scenario: Transport failover — CXI to TCP
    Given the native client is using libfabric/CXI
    When the CXI transport fails (NIC issue, fabric partition)
    Then the client falls back to TCP transport
    And operations continue at reduced performance
    And the client periodically attempts to reconnect via CXI
    And the failover is transparent to the workload

  # --- Discovery protocol (ADR-008) ---

  @integration
  Scenario: All seed endpoints unreachable — discovery fails
    Given the native client is configured with seed list [node1:9100, node2:9100]
    And both seed endpoints are unreachable
    When the native client attempts to initialize
    Then discovery fails with retriable "no seeds reachable" error
    And the client retries with exponential backoff
    And the workload receives EIO until discovery succeeds

  @integration
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

  @integration
  Scenario: Multiple clients writing to the same file concurrently
    Given two native client instances on different compute nodes
    And both write to /mnt/kiseki/trials/shared-log.txt
    Then writes from both clients are serialized in the shard (Raft ordering)
    And the final state reflects a total order of all writes
    And neither client's writes are lost (though interleaving is possible)

  @unit
  Scenario: FUSE mount with read-only namespace
    Given namespace "archive" is marked read-only in the control plane
    When the native client mounts /mnt/kiseki/archive
    Then reads succeed normally
    And writes return EROFS (read-only filesystem)

  # --- Workflow Advisory integration (ADR-020) ---
  # The native client is the ORIGINATOR of advisory hints and the CONSUMER
  # of telemetry feedback. Full lifecycle/invariant scenarios live in
  # workflow-advisory.feature; scenarios here cover integration with the
  # existing FUSE/native read/write/caching paths.

  @unit
  Scenario: Client declares a workflow and correlates subsequent operations
    Given the native client is initialized under workload "training-run-42"
    When the workload calls kiseki_declare_workflow(profile="ai-training", initial_phase="stage-in")
    Then the client obtains an opaque WorkflowSession handle
    And all subsequent read/write calls that take an optional session argument carry the workflow_ref annotation
    And operations without a session argument continue to work unchanged (advisory annotation absent, I-WA1/I-WA2)

  @unit
  Scenario: Pattern-detector origin — access-pattern hint emitted on detected sequential read
    Given the workflow is in phase "stage-in" with profile ai-training
    And the native client's pattern detector observes three consecutive sequential reads on /mnt/kiseki/trials/dataset.h5
    When the detector classifies the access as sequential
    Then the client submits hint { access_pattern: sequential, target: composition_id of dataset.h5 } on the advisory channel
    And continues to serve reads normally (hint emission is asynchronous and non-blocking, I-WA2)
    And if the advisory channel is unavailable the read path is unaffected

  @unit
  Scenario: Client declares prefetch ranges for an AI shuffled epoch
    Given the workflow advances to phase "epoch-0"
    When the workload computes the shuffled read order and calls kiseki_declare_prefetch(tuples)
    Then the client batches tuples into PrefetchHint messages each under max_prefetch_tuples_per_hint (I-WA16)
    And submits them on the advisory channel
    And subsequent FUSE reads in the predicted order benefit from warmed cache (measured via prefetch-effectiveness telemetry)

  @unit
  Scenario: Client throttles itself on hard backpressure telemetry
    Given the workflow is subscribed to backpressure telemetry on pool "fast-nvme"
    When the client receives a backpressure event with severity "hard" and retry_after_ms 250
    Then the client MAY pause or rate-limit new submissions for ≈ retry_after_ms
    And correctness of in-flight operations is unaffected (I-WA1)
    And actual quota enforcement remains the data path's responsibility (I-T2)

  @unit
  Scenario: Advisory disabled at workload level — client degrades gracefully
    Given tenant admin disables Workflow Advisory for "training-run-42"
    When the client calls kiseki_declare_workflow
    Then the call returns ADVISORY_DISABLED
    And the client falls back to pattern-inference for access-pattern heuristics
    And FUSE reads and writes are fully correct and at normal performance (I-WA12)

  # =====================================================================
  # Client-side cache (ADR-031)
  # =====================================================================

  @unit
  Scenario: Pinned mode stages a dataset
    Given a client with cache_mode "pinned" and staging_enabled true
    When the client runs "kiseki-client stage --dataset /training/imagenet"
    Then all compositions under "/training/imagenet" are enumerated recursively
    And each chunk is fetched from canonical, verified (SHA-256), and stored in L2
    And a staging manifest is written listing all compositions and chunk_ids
    And staged chunks are retained against LRU eviction

  @unit
  Scenario: Staging handoff from prolog to workload
    Given a staging daemon has populated an L2 pool with pool_id "abc"
    And the staging daemon holds the pool.lock flock
    When a workload process starts with KISEKI_CACHE_POOL_ID="abc"
    Then the workload adopts the existing pool instead of creating a new one
    And the workload takes over the flock
    And the staging daemon exits cleanly

  @unit
  Scenario: Staging beyond capacity returns error
    Given a client with cache_mode "pinned" and max_cache_bytes 10GB
    And 9GB is already staged
    When the client stages a 5GB dataset
    Then the staging returns CacheCapacityExceeded
    And no existing pinned data is evicted

  @integration
  Scenario: Cache policy resolved via data-path gRPC
    Given a compute node with no gateway or control plane access
    And the client connects to a storage node via data-path gRPC
    When the client establishes a session
    Then cache policy is fetched via GetCachePolicy RPC on the data-path channel (I-CC9)
    And the client operates within the policy ceilings

  @unit
  Scenario: Per-node cache capacity enforcement
    Given 5 concurrent client processes for the same tenant on one node
    And max_node_cache_bytes is set to 200GB
    When the 5th process attempts to insert into L2 and total usage exceeds 200GB
    Then the insert is rejected
    And organic mode triggers additional eviction before retrying
