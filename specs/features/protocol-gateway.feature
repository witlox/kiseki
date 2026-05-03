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
    And the completed object contains all parts' data concatenated

  # --- Protocol semantics enforcement ---

  @library
  Scenario: NFSv4.1 state management — open/lock
    Given a client opens "/trials/shared.log" with NFS OPEN
    And acquires an NFS byte-range lock on bytes 0-1024
    When another client attempts to lock the same range
    Then the second lock is denied (NFS mandatory locking semantics)
    And the gateway maintains lock state per client session
    And lock state is gateway-local (not replicated to other gateways)

  # S3 PUT honors `If-None-Match: *` over the wire: a second PUT to
  # the same key returns 412 Precondition Failed. Backed by the
  # gateway's per-bucket name index (kiseki_composition::
  # CompositionStorage name index — forward + reverse maps,
  # replicated to followers via the Create delta's v2 payload's
  # `name` field).
  @integration
  Scenario: S3 conditional write — If-None-Match
    Given object "results/v2.json" does not exist
    When a client issues PutObject with header If-None-Match: *
    Then the write succeeds
    And if the object already existed, the write would return 412 Precondition Failed

  # GET-by-key + DELETE-by-key + LIST require per-key naming. The
  # in-memory test path used to fake these by parsing the URL key as
  # a UUID; the real product needs a per-bucket key→composition_id
  # index. Each step issues HTTP against w.server() and asserts on
  # the wire behavior of the running kiseki-server.
  @integration
  Scenario: S3 round-trip by URL key — PUT, GET, HEAD, DELETE
    When a client S3 PUTs "alpha-payload" to key "alpha/file.bin"
    Then a S3 GET on "alpha/file.bin" returns "alpha-payload"
    And a S3 HEAD on "alpha/file.bin" returns content-length 13
    And a S3 DELETE on "alpha/file.bin" returns 204
    And a S3 GET on "alpha/file.bin" returns 404

  # Multi-node read-after-write by URL key. The Create delta carries
  # the name field (v2 payload), the hydrator on every follower
  # replays it into the redb name index, and a GET on a non-leader
  # node resolves the same key. Failure here means the v2 payload
  # isn't being decoded or the name_inserts batch isn't being
  # committed atomically with the composition put — both of which
  # would silently break key-based addressing on a multi-node cluster.
  @integration @multi-node
  Scenario: 6-node cluster — S3 GET-by-key resolves on follower after PUT to leader
    Given a 6-node kiseki cluster
    When a client S3 PUTs "cross-node-payload" to key "x/replicated.bin" on node-1
    Then a S3 GET for key "x/replicated.bin" on node-2 returns "cross-node-payload"
    And a S3 GET for key "x/replicated.bin" on node-3 returns "cross-node-payload"

  @integration
  Scenario: S3 LIST returns bucket contents by URL key
    When a client S3 PUTs "one" to key "lst/one.txt"
    And a client S3 PUTs "two" to key "lst/two.txt"
    And a client S3 PUTs "three" to key "lst/three.txt"
    Then a S3 LIST with prefix "lst/" returns keys "lst/one.txt, lst/three.txt, lst/two.txt"

  # FUSE in BDD without a kernel mount: drive `KisekiFuse` (the
  # POSIX → GatewayOps translator) against a `RemoteHttpGateway`
  # pointed at the running server's S3 port. Proves the
  # client-side FUSE→gateway→wire path end-to-end. Kernel-mount
  # coverage stays in python e2e (`tests/e2e/test_fuse.py`) per
  # the @e2e-deferred convention.
  @integration
  Scenario: FUSE → GatewayOps → S3 wire roundtrip
    When the FUSE filesystem (backed by RemoteHttpGateway) creates "/fuse-rt.bin" with payload "fuse-payload"
    Then the FUSE filesystem read of "/fuse-rt.bin" returns "fuse-payload"
    And the FUSE filesystem unlink of "/fuse-rt.bin" succeeds
    And the FUSE filesystem read of "/fuse-rt.bin" returns ENOENT

  # Operational metrics smoke. The GCP 2026-05-02 perf cluster
  # reported `kiseki_gateway_requests_total = 0` after 1 GB of
  # writes — i.e. the metric was not wired. Asserts the counter
  # increments on every PUT (and chunk metrics on every payload
  # > 0) so a future regression of the wiring fails the BDD suite
  # before the GCP perf run.
  @integration
  Scenario: Server /metrics surfaces non-zero counters after a real workload
    Given the gateway counters are baselined
    When a 4KB object is PUT and immediately GET via S3
    Then kiseki_gateway_requests_total has incremented since the baseline
    And kiseki_chunk_write_bytes_total has incremented since the baseline
    And kiseki_chunk_read_bytes_total has incremented since the baseline

  # Multipart upload + per-key naming. Without per-key naming on
  # CompleteMultipartUpload, multipart-uploaded objects would be
  # addressable only by their composition UUID — the per-key code
  # path for plain PUT would silently bypass them. This scenario
  # PUTs three parts via the multipart API and asserts subsequent
  # GET-by-key resolves the same content. Closes the asymmetry.
  @integration
  Scenario: S3 multipart upload binds the URL key
    When a client multipart-uploads "alpha-bravo-charlie" to key "mp/composed.bin" in 3 parts
    Then a S3 GET on "mp/composed.bin" returns "alpha-bravo-charlie"
    And a S3 DELETE on "mp/composed.bin" returns 204
    And a S3 GET on "mp/composed.bin" returns 404

  # Multi-node multipart correctness: the leader's multipart
  # upload's name binding + every part's chunk_state must replicate
  # to followers via the Raft Create-delta (v2 payload + new_chunks
  # list). Without that, a GET-by-key on a follower would 404 (no
  # name binding) or `ChunkLost` (no cluster_chunk_state seed for
  # the multipart's chunks).
  @integration @multi-node
  Scenario: 6-node cluster — multipart upload by key resolves on followers
    Given a 6-node kiseki cluster
    When a client multipart-uploads "leader-mp-payload" to key "mp/multi.bin" in 3 parts on node-1
    Then a S3 GET for key "mp/multi.bin" on node-2 returns "leader-mp-payload"
    And a S3 GET for key "mp/multi.bin" on node-3 returns "leader-mp-payload"

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

  @library
  Scenario: Gateway crash — client reconnects
    Given "gw-nfs-pharma" crashes
    When the gateway is restarted (or a new instance spun up)
    Then NFS clients detect connection loss
    And clients reconnect to the new gateway instance
    And NFS state (opens, locks) is lost — clients re-establish
    And no committed data is lost (durability is in the Log + Chunk Storage)
    And in-flight uncommitted writes are lost

  @library
  Scenario: Gateway cannot reach tenant KMS — writes fail
    Given tenant KMS for "org-pharma" is unreachable
    And cached KEK has expired
    When a write arrives at "gw-nfs-pharma"
    Then the gateway cannot encrypt for the tenant
    And the write is rejected with a retriable error
    And reads of previously cached/materialized data may still work
    And the tenant admin is alerted

  @library
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

  # ADR-021 / I-WA1: `x-kiseki-workflow-ref` is an advisory header.
  # The data-path validates it against the running server's
  # `WorkflowTable` (shared with the AdvisoryService gRPC). Three
  # outcomes — `valid`, `invalid`, `absent` — each tick a labeled
  # counter on `/metrics`. Per I-WA1, an unknown ref must NOT block
  # the write — only the counter result differs.
  @integration
  Scenario: S3 request carries workflow_ref header to advisory
    Given a workflow "training-run-42" declared via advisory gRPC
    When a S3 PUT arrives with the workflow_ref header set to the declared workflow
    Then the metric kiseki_gateway_workflow_ref_writes_total{result="valid"} increments
    When a S3 PUT arrives with the workflow_ref header set to a random uuid
    Then the write succeeds (header is advisory — I-WA1)
    And the metric kiseki_gateway_workflow_ref_writes_total{result="invalid"} increments
    When a S3 PUT arrives without the workflow_ref header
    Then the metric kiseki_gateway_workflow_ref_writes_total{result="absent"} increments

  @library
  Scenario: Priority-class hint applied to request scheduling within policy
    Given workload "training-run-42"'s allowed priority classes are [batch, bulk]
    And the client's hint carries { priority: batch }
    When the gateway schedules the request against concurrent workload traffic
    Then the request is placed in the batch QoS class
    And a hint requesting { priority: interactive } is rejected with hint-rejected reason "priority_not_allowed" without affecting the underlying request (I-WA14)

  @library
  Scenario: Request-level backpressure telemetry emitted on sustained saturation
    Given the gateway serves "training-run-42" with 200 concurrent in-flight requests
    And the workload has subscribed to backpressure telemetry
    When the gateway's per-caller queue depth crosses the soft threshold
    Then a backpressure event { severity: soft, retry_after_ms: <bucketed> } is emitted to the workflow (I-WA5)
    And only the caller's own queue state contributes to the signal; neighbour callers do not leak through this channel (I-WA5)
    And data-path requests continue to be accepted

  @library
  Scenario: Access-pattern hint routed from protocol metadata
    Given an NFSv4.1 client submits read with `io_advise` hints indicating sequential access
    When the gateway maps the advisory to a Workflow Advisory hint { access_pattern: sequential }
    Then the advisory is submitted asynchronously (I-WA2) and the NFS read is served normally
    And the View Materialization subsystem MAY readahead for subsequent reads of the same caller

  @library
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

  @library
  Scenario: QoS-headroom telemetry caller-scoped
    Given workload "training-run-42" is subscribed to QoS-headroom telemetry
    When the gateway computes headroom within the workload's I-T2 quota
    Then the value is a bucketed fraction ∈ {ample, moderate, tight, exhausted}
    And no neighbour workload's headroom is disclosed (I-WA5)
