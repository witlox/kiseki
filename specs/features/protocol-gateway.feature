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

  # --- NFS read path ---

  Scenario: NFS READ — serve from materialized view
    Given a client issues NFS READ for "/trials/results.h5" offset 0 length 64MB
    When "gw-nfs-pharma" receives the request
    Then it resolves the path in the NFS view "nfs-trials"
    And identifies the chunk references for the requested byte range
    And reads encrypted chunks from Chunk Storage
    And unwraps system DEK via tenant KEK
    And decrypts chunks to plaintext
    And returns plaintext to the NFS client over TLS
    And plaintext exists only in gateway memory, ephemerally

  Scenario: NFS READDIR — directory listing from view
    Given a client issues NFS READDIR for "/trials/"
    When "gw-nfs-pharma" receives the request
    Then it reads the directory listing from the NFS view
    And the view contains decrypted filenames (stream processor decrypted them)
    And returns the listing to the client over TLS

  # --- NFS write path ---

  Scenario: NFS WRITE — encrypt and commit through Composition
    Given a client issues NFS WRITE for "/trials/new-data.bin" with 128MB of data
    When "gw-nfs-pharma" receives the plaintext over TLS
    Then the gateway:
      | step | action                                              |
      | 1    | chunks the plaintext (content-defined, variable-size)|
      | 2    | computes chunk_id = sha256(plaintext) per chunk      |
      | 3    | submits chunks to Chunk Storage (system encrypts)    |
      | 4    | receives ChunkStored confirmations                   |
      | 5    | submits delta to Composition context                 |
      | 6    | Composition appends delta to shard                   |
      | 7    | receives DeltaCommitted                              |
    And the gateway returns NFS WRITE success to the client
    And plaintext is discarded from gateway memory after step 2

  Scenario: NFS CREATE — small file with inline data
    Given a client creates a 256-byte file via NFS
    When "gw-nfs-pharma" receives the data
    Then the gateway encrypts the data for the delta payload
    And submits to Composition with inline data (below threshold)
    And no chunk write occurs
    And the delta commits with inline encrypted payload

  # --- S3 read path ---

  Scenario: S3 GetObject — serve from S3 view
    Given a client issues S3 GetObject for "s3://trials/dataset.parquet"
    When "gw-s3-pharma" receives the request
    Then it resolves the object key in the S3 view "s3-trials"
    And reads encrypted chunks from Chunk Storage
    And decrypts using tenant KEK → system DEK
    And returns plaintext as S3 response body over TLS

  Scenario: S3 ListObjectsV2 — bucket listing from view
    Given a client issues S3 ListObjectsV2 for bucket "trials" with prefix "study-42/"
    When "gw-s3-pharma" receives the request
    Then it reads the object listing from the S3 view
    And returns matching keys, sizes, and last-modified timestamps
    And the listing reflects the S3 view's current watermark (bounded-staleness)

  # --- S3 write path ---

  Scenario: S3 PutObject — single-part upload
    Given a client issues S3 PutObject for "results/output.csv" with 10MB body
    When "gw-s3-pharma" receives the plaintext over TLS
    Then the gateway chunks, computes chunk_ids, writes chunks, commits delta
    And returns S3 200 OK with ETag
    And the object is visible in the S3 view after the stream processor consumes the delta

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

  Scenario: NFSv4.1 state management — open/lock
    Given a client opens "/trials/shared.log" with NFS OPEN
    And acquires an NFS byte-range lock on bytes 0-1024
    When another client attempts to lock the same range
    Then the second lock is denied (NFS mandatory locking semantics)
    And the gateway maintains lock state per client session
    And lock state is gateway-local (not replicated to other gateways)

  Scenario: S3 conditional write — If-None-Match
    Given object "results/v2.json" does not exist
    When a client issues PutObject with header If-None-Match: *
    Then the write succeeds
    And if the object already existed, the write would return 412 Precondition Failed

  # --- Transport pluggability ---

  Scenario: NFS gateway over TCP
    Given "gw-nfs-pharma" is configured with transport TCP
    When a client connects
    Then NFS traffic flows over TCP with TLS encryption
    And the gateway handles NFS RPC framing over TCP

  Scenario: S3 gateway over TCP (HTTPS)
    Given "gw-s3-pharma" is configured with transport TCP
    When a client connects
    Then S3 traffic flows over HTTPS (TLS)
    And standard S3 REST API semantics apply

  # --- Failure paths ---

  Scenario: Gateway crash — client reconnects
    Given "gw-nfs-pharma" crashes
    When the gateway is restarted (or a new instance spun up)
    Then NFS clients detect connection loss
    And clients reconnect to the new gateway instance
    And NFS state (opens, locks) is lost — clients re-establish
    And no committed data is lost (durability is in the Log + Chunk Storage)
    And in-flight uncommitted writes are lost

  Scenario: Gateway cannot reach tenant KMS — writes fail
    Given tenant KMS for "org-pharma" is unreachable
    And cached KEK has expired
    When a write arrives at "gw-nfs-pharma"
    Then the gateway cannot encrypt for the tenant
    And the write is rejected with a retriable error
    And reads of previously cached/materialized data may still work
    And the tenant admin is alerted

  Scenario: Gateway cannot reach Chunk Storage — read fails
    Given Chunk Storage is partially unavailable
    When a read requests a chunk on an unavailable device
    Then EC repair is attempted if parity is available
    And if repair succeeds, the read completes
    And if repair fails, the read returns an error to the client
    And the error is protocol-appropriate (NFS: EIO, S3: 500 Internal Server Error)

  Scenario: Gateway receives request for wrong tenant
    Given "gw-nfs-pharma" serves only tenant "org-pharma"
    When a request arrives with credentials for "org-biotech"
    Then the request is rejected with authentication error
    And the attempt is recorded in the audit log
    And no data from "org-pharma" is exposed

  # --- Workflow Advisory integration (ADR-020) ---
  # Gateways act on priority-class, access-pattern, and deadline hints
  # and emit request-level backpressure and QoS-headroom telemetry to the
  # caller's workflow. Protocol clients (NFS/S3) carry the workflow_ref
  # via a lightweight header; correlation to a workflow is optional and
  # never a precondition for the request (I-WA1, I-WA2).

  Scenario: S3 request carries workflow_ref header to advisory
    Given S3 client under workload "training-run-42" has an active workflow
    When a PutObject arrives with header `x-kiseki-workflow-ref: <opaque>`
    Then the gateway validates the ref against the authenticated tenant identity (I-WA3)
    And on success, annotates the write path for advisory correlation
    And on mismatch or unknown ref, ignores the header silently and processes the request unchanged (I-WA1)

  Scenario: Priority-class hint applied to request scheduling within policy
    Given workload "training-run-42"'s allowed priority classes are [batch, bulk]
    And the client's hint carries { priority: batch }
    When the gateway schedules the request against concurrent workload traffic
    Then the request is placed in the batch QoS class
    And a hint requesting { priority: interactive } is rejected with hint-rejected reason "priority_not_allowed" without affecting the underlying request (I-WA14)

  Scenario: Request-level backpressure telemetry emitted on sustained saturation
    Given the gateway serves "training-run-42" with 200 concurrent in-flight requests
    And the workload has subscribed to backpressure telemetry
    When the gateway's per-caller queue depth crosses the soft threshold
    Then a backpressure event { severity: soft, retry_after_ms: <bucketed> } is emitted to the workflow (I-WA5)
    And only the caller's own queue state contributes to the signal; neighbour callers do not leak through this channel (I-WA5)
    And data-path requests continue to be accepted

  Scenario: Access-pattern hint routed from protocol metadata
    Given an NFSv4.1 client submits read with `io_advise` hints indicating sequential access
    When the gateway maps the advisory to a Workflow Advisory hint { access_pattern: sequential }
    Then the advisory is submitted asynchronously (I-WA2) and the NFS read is served normally
    And the View Materialization subsystem MAY readahead for subsequent reads of the same caller

  Scenario: Advisory disabled at workload — gateway ignores hints, serves protocol normally
    Given tenant admin transitions "training-run-42" advisory to disabled
    When NFS or S3 requests arrive with workflow_ref or priority hints
    Then the gateway ignores all advisory annotations
    And serves the request with default scheduling and protocol semantics
    And no performance or correctness regression is observable (I-WA12)

  Scenario: QoS-headroom telemetry caller-scoped
    Given workload "training-run-42" is subscribed to QoS-headroom telemetry
    When the gateway computes headroom within the workload's I-T2 quota
    Then the value is a bucketed fraction ∈ {ample, moderate, tight, exhausted}
    And no neighbour workload's headroom is disclosed (I-WA5)
