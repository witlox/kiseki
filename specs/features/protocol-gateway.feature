Feature: Protocol Gateway — Wire protocol translation and tenant-layer encryption
  The Protocol Gateway translates NFS/S3 wire protocol requests into
  operations against views (reads) and the Composition context (writes).
  Performs tenant-layer encryption for protocol-path clients. Clients
  send plaintext over TLS; the gateway encrypts before writing.

  Background:
    Given a Kiseki cluster with tenant "org-pharma"
    And NFS gateway "gw-nfs-pharma" serving tenant "org-pharma"
    And S3 gateway "gw-s3-pharma" serving tenant "org-pharma"
    And tenant KEK "pharma-kek-001" cached in both gateways
    And NFS view "nfs-trials" at watermark 5000
    And S3 view "s3-trials" at watermark 4998

  @integration
  Scenario: S3 multipart upload — large object
    Given a client starts S3 CreateMultipartUpload for "checkpoints/epoch-100.pt"
    When parts are uploaded:
      | part | size  | chunk_ids    |
      | 1    | 100MB | [c1, c2]     |
      | 2    | 100MB | [c3, c4]     |
      | 3    | 50MB  | [c5]         |
    And the client sends CompleteMultipartUpload
    Then the gateway verifies all chunks are durable
    And submits a finalize delta to Composition
    And the object becomes visible only after finalize commits (I-L5)
    And parts are NOT visible individually before completion

  # --- Protocol semantics enforcement ---

  @integration
  Scenario: NFSv4.1 state management — open/lock
    Given a client opens "/trials/shared.log" with NFS OPEN
    And acquires an NFS byte-range lock on bytes 0-1024
    When another client attempts to lock the same range
    Then the second lock is denied (NFS mandatory locking semantics)
    And the gateway maintains lock state per client session
    And lock state is gateway-local (not replicated to other gateways)

  @integration
  Scenario: S3 conditional write — If-None-Match
    Given object "results/v2.json" does not exist
    When a client issues PutObject with header If-None-Match: *
    Then the write succeeds
    And if the object already existed, the write would return 412 Precondition Failed

  # --- Transport pluggability ---

  @integration
  Scenario: NFS gateway over TCP
    Given "gw-nfs-pharma" is configured with transport TCP
    When a client connects
    Then NFS traffic flows over TCP with TLS encryption
    And the gateway handles NFS RPC framing over TCP

  @integration
  Scenario: S3 gateway over TCP (HTTPS)
    Given "gw-s3-pharma" is configured with transport TCP
    When a client connects
    Then S3 traffic flows over HTTPS (TLS)
    And standard S3 REST API semantics apply

  # --- Failure paths ---

  @integration
  Scenario: Gateway crash — client reconnects
    Given "gw-nfs-pharma" crashes
    When the gateway is restarted (or a new instance spun up)
    Then NFS clients detect connection loss
    And clients reconnect to the new gateway instance
    And NFS state (opens, locks) is lost — clients re-establish
    And no committed data is lost (durability is in the Log + Chunk Storage)
    And in-flight uncommitted writes are lost

  @integration
  Scenario: Gateway cannot reach tenant KMS — writes fail
    Given tenant KMS for "org-pharma" is unreachable
    And cached KEK has expired
    When a write arrives at "gw-nfs-pharma"
    Then the gateway cannot encrypt for the tenant
    And the write is rejected with a retriable error
    And reads of previously cached/materialized data may still work
    And the tenant admin is alerted

  @integration
  Scenario: Gateway cannot reach Chunk Storage — read fails
    Given Chunk Storage is partially unavailable
    When a read requests a chunk on an unavailable device
    Then EC repair is attempted if parity is available
    And if repair succeeds, the read completes
    And if repair fails, the read returns an error to the client
    And the error is protocol-appropriate (NFS: EIO, S3: 500 Internal Server Error)

  # --- Workflow Advisory integration (ADR-020) ---
  # Gateways act on priority-class, access-pattern, and deadline hints
  # and emit request-level backpressure and QoS-headroom telemetry to the
  # caller's workflow. Protocol clients (NFS/S3) carry the workflow_ref
  # via a lightweight header; correlation to a workflow is optional and
  # never a precondition for the request (I-WA1, I-WA2).

  @integration
  Scenario: S3 request carries workflow_ref header to advisory
    Given S3 client under workload "training-run-42" has an active workflow
    When a PutObject arrives with header `x-kiseki-workflow-ref: <opaque>`
    Then the gateway validates the ref against the authenticated tenant identity (I-WA3)
    And on success, annotates the write path for advisory correlation
    And on mismatch or unknown ref, ignores the header silently and processes the request unchanged (I-WA1)

  @integration
  Scenario: Priority-class hint applied to request scheduling within policy
    Given workload "training-run-42"'s allowed priority classes are [batch, bulk]
    And the client's hint carries { priority: batch }
    When the gateway schedules the request against concurrent workload traffic
    Then the request is placed in the batch QoS class
    And a hint requesting { priority: interactive } is rejected with hint-rejected reason "priority_not_allowed" without affecting the underlying request (I-WA14)

  @integration
  Scenario: Request-level backpressure telemetry emitted on sustained saturation
    Given the gateway serves "training-run-42" with 200 concurrent in-flight requests
    And the workload has subscribed to backpressure telemetry
    When the gateway's per-caller queue depth crosses the soft threshold
    Then a backpressure event { severity: soft, retry_after_ms: <bucketed> } is emitted to the workflow (I-WA5)
    And only the caller's own queue state contributes to the signal; neighbour callers do not leak through this channel (I-WA5)
    And data-path requests continue to be accepted

  @integration
  Scenario: Access-pattern hint routed from protocol metadata
    Given an NFSv4.1 client submits read with `io_advise` hints indicating sequential access
    When the gateway maps the advisory to a Workflow Advisory hint { access_pattern: sequential }
    Then the advisory is submitted asynchronously (I-WA2) and the NFS read is served normally
    And the View Materialization subsystem MAY readahead for subsequent reads of the same caller

  @integration
  Scenario: NFS workflow_ref carriage model (v1)
    Given NFSv4.1 is a POSIX-oriented protocol with no native header for workflow correlation
    When a workload mounts an NFS export via "gw-nfs-pharma"
    Then workflow correlation for NFS clients is attached per-mount by the gateway:
      | option              | value                              |
      | mount_option        | `workflow-ref=<16-byte-hex>`        |
      | mount_option_source | workload's DeclareWorkflow response |
    And all RPCs on that mount inherit that workflow_ref internally (translated to the gRPC binary header at the kiseki-server ingress)
    And mounts without `workflow-ref` proceed with no advisory correlation — data-path behavior is identical to pre-advisory NFS (I-WA1, I-WA2)
    And the gateway MAY refuse a mount whose workflow_ref is unknown or belongs to a different workload; that refusal is a mount-time error, not mid-session

  @integration
  Scenario: QoS-headroom telemetry caller-scoped
    Given workload "training-run-42" is subscribed to QoS-headroom telemetry
    When the gateway computes headroom within the workload's I-T2 quota
    Then the value is a bucketed fraction ∈ {ample, moderate, tight, exhausted}
    And no neighbour workload's headroom is disclosed (I-WA5)
