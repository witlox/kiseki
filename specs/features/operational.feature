Feature: Operational — Integrity monitoring, schema versioning, compression, observability
  Cross-cutting operational concerns that span multiple bounded contexts.

  Background:
    Given a Kiseki cluster with 5 storage nodes
    And tenant "org-pharma" with compliance tags [HIPAA, GDPR]
    And system key manager healthy at epoch 3

  # --- Runtime integrity monitor (ADR-018, I-O7) ---

  @unit
  Scenario: ptrace attachment detected on kiseki-server process
    Given kiseki-server is running on node 1 with PID 12345
    And the integrity monitor is watching PID 12345
    When an external process attaches via ptrace to PID 12345
    Then the monitor detects TracerPid != 0 in /proc/12345/status
    And an alert is sent to the cluster admin (critical severity)
    And an alert is sent to all tenant admins with data on node 1
    And the event is recorded in the audit log
    And if auto-rotate is enabled: system master key rotation is triggered

  @unit
  Scenario: Core dump attempt blocked
    Given kiseki-server has core dumps disabled (RLIMIT_CORE=0, MADV_DONTDUMP)
    When a SIGABRT is received by the process
    Then no core dump is generated
    And key material in mlock'd pages is not written to disk
    And the event is recorded in the audit log

  @unit
  Scenario: Integrity monitor in development mode
    Given the cluster is in development/test mode
    And the integrity monitor is configured as disabled
    Then ptrace attachments do not trigger alerts
    And debuggers can attach normally
    And this mode is NOT available in production configuration

  # --- Schema versioning (ADR-004) ---

  @unit
  Scenario: New-version stream processor reads old-format deltas
    Given shard "shard-1" contains deltas in format version 1
    And a new stream processor supports format versions [1, 2]
    When the stream processor consumes deltas from shard-1
    Then it reads format version 1 deltas successfully
    And materializes the view correctly
    And no upgrade of the delta format is required

  @unit
  Scenario: Old-version stream processor encounters unknown format
    Given shard "shard-1" contains a delta in format version 3
    And the stream processor supports format versions [1, 2] only
    When the stream processor encounters the version 3 delta
    Then it skips the delta with a warning log
    And continues processing subsequent deltas
    And the skipped delta is flagged for manual review
    And the view may have a gap (documented behavior)

  @integration
  Scenario: Rolling upgrade — mixed version cluster
    Given nodes [1, 2, 3] are running kiseki-server v1.0 (format version 1)
    When node 1 is upgraded to v1.1 (supports format versions [1, 2])
    Then node 1 reads format v1 deltas from other nodes
    And node 1 writes format v1 deltas (not v2, until all nodes upgraded)
    And Raft replication works across mixed versions
    And after all nodes upgraded: writers switch to format v2

  @unit
  Scenario: Chunk envelope version preserved through compaction
    Given shard "shard-1" has deltas with format versions [1, 1, 2, 2]
    When compaction merges these deltas
    Then each delta retains its original format version
    And compaction does not upgrade delta formats
    And encrypted payloads are carried opaquely regardless of version

  # --- Compression (I-K14) ---

  @unit
  Scenario: Tenant opts in to compression
    Given "org-biotech" has no HIPAA compliance tag
    When the tenant admin enables compression for "org-biotech"
    Then new chunks are compressed before encryption
    And compressed data is padded to 4KB alignment before encryption
    And the chunk metadata records compressed=true
    And existing chunks are NOT retroactively compressed

  @unit
  Scenario: Compressed chunk round-trip
    Given "org-biotech" has compression enabled
    When a 10MB plaintext file is written
    Then the plaintext is compressed (e.g., zstd)
    And padded to 4KB alignment
    And encrypted with system DEK
    And stored as a chunk with compressed=true
    When the chunk is read
    Then the ciphertext is decrypted
    And decompressed to recover the original 10MB plaintext

  @unit
  Scenario: HIPAA namespace blocks compression opt-in
    Given "org-pharma" has compliance tag [HIPAA]
    When the tenant admin attempts to enable compression
    Then the request is rejected with "compression prohibited by HIPAA compliance tag"
    And no compression setting is changed

  @unit
  Scenario: Compression disabled by default
    Given a new tenant "org-newco" is created with default settings
    Then compression is disabled
    And all chunks are stored without compression

  # --- Audit GC safety valve (ADR-009 revised, I-A5) ---

  @unit
  Scenario: Audit export stalls — safety valve triggers GC
    Given "org-pharma" audit export has stalled for 25 hours
    And the safety valve threshold is 24 hours
    And shard "shard-trials-1" has deltas eligible for GC
    When the GC process evaluates "shard-trials-1"
    Then GC proceeds despite the stalled audit watermark
    And an audit gap is recorded in the audit log
    And the compliance team is notified of the gap
    And storage is reclaimed

  @unit
  Scenario: Audit backpressure mode — writes throttled
    Given "org-pharma" has audit backpressure mode enabled
    And "org-pharma" audit export is falling behind
    When write pressure exceeds the audit consumption rate
    Then write throughput for "org-pharma" is throttled
    And the audit log catches up
    And no audit gap occurs
    And the tenant admin is notified of throttled writes

  @unit
  Scenario: Audit backpressure does not affect other tenants
    Given "org-pharma" has backpressure mode and is being throttled
    And "org-biotech" has default safety valve mode
    When "org-biotech" writes data
    Then "org-biotech" writes proceed at full speed
    And "org-pharma" throttling is tenant-scoped only

  # --- Retention hold auto-creation (ADR-010) ---

  @unit
  Scenario: HIPAA namespace auto-creates retention hold
    Given tenant admin creates namespace "patient-records" with tag [HIPAA]
    When the namespace is created
    Then a default retention hold is automatically created
    And the hold TTL is 6 years (HIPAA §164.530(j))
    And the hold is recorded in the audit log
    And the tenant admin is notified of the auto-hold

  @unit
  Scenario: Crypto-shred blocked when compliance implies retention
    Given namespace "patient-records" has tag [HIPAA]
    And no explicit retention hold exists (auto-hold was not created — edge case)
    When "org-pharma" attempts crypto-shred
    Then crypto-shred is blocked with error: "compliance tags imply retention; set hold or use force override"
    And the block is recorded in the audit log

  @unit
  Scenario: Crypto-shred with force override — audited
    Given namespace "patient-records" has HIPAA tag but no retention hold
    When "org-pharma" performs crypto-shred with force_without_hold_check=true
    Then crypto-shred proceeds (KEK destroyed)
    And an audit event records the override with reason
    And the compliance team is alerted to the forced shred

  # --- Crypto-shred invalidation broadcast (ADR-011, I-K15) ---

  @integration
  Scenario: Crypto-shred triggers invalidation broadcast
    Given gateways [gw-1, gw-2] and stream processors [sp-1, sp-2] cache "org-pharma" KEK
    When crypto-shred is executed for "org-pharma"
    Then an invalidation broadcast is sent to [gw-1, gw-2, sp-1, sp-2]
    And components receiving the broadcast immediately purge cached KEK
    And crypto-shred returns success after KEK destruction + broadcast
    And it does NOT wait for all acknowledgments

  @integration
  Scenario: Unreachable component — TTL expires naturally
    Given native client "client-1" on an unreachable compute node caches "org-pharma" KEK
    And the cache TTL is 60 seconds
    When crypto-shred is executed and invalidation broadcast sent
    And "client-1" does not receive the broadcast
    Then "client-1" can still decrypt data for up to 60 seconds
    And after 60 seconds, the cached KEK expires
    And subsequent operations from "client-1" fail with "key unavailable"

  @unit
  Scenario: Tenant configures shorter crypto-shred TTL
    Given "org-pharma" requests cache TTL of 10 seconds (within [5s, 300s] bounds)
    When the control plane processes the request
    Then the TTL is set to 10 seconds for all "org-pharma" key caches
    And KMS load increases (key refresh every 10 seconds per component)
    And the configuration change is recorded in the audit log

  @unit
  Scenario: TTL below minimum rejected
    Given "org-pharma" requests cache TTL of 2 seconds
    When the control plane processes the request
    Then the request is rejected with "TTL below minimum (5s)"
    And the current TTL is unchanged

  # --- Writable mmap (ADR-013, I-O8) ---

  @unit
  Scenario: Writable shared mmap returns clear error
    Given a workload opens a file via FUSE mount
    When the workload calls mmap with PROT_WRITE and MAP_SHARED
    Then the native client returns ENOTSUP
    And logs: "writable shared mmap not supported; use write() instead"
    And the workload receives the error immediately

  @unit
  Scenario: Read-only mmap works
    Given a workload opens a file via FUSE mount
    When the workload calls mmap with PROT_READ and MAP_PRIVATE
    Then the mmap succeeds
    And the file contents are readable through the mapped region
    And this is useful for model loading and read-only data access

  # --- Multi-endpoint client resilience (ADR-019, I-O9) ---

  @integration
  Scenario: NFS client reconnects after node failure
    Given an NFS client is connected to gateway on node 1
    And the NFS mount is configured with multiple server addresses [node1, node2, node3]
    When node 1 crashes
    Then the NFS client detects connection loss
    And reconnects to node 2 or node 3 automatically
    And NFS operations resume (session state re-established)

  @integration
  Scenario: S3 client retries to different endpoint on error
    Given an S3 client sends PutObject to node 1
    And node 1 returns 503 Service Unavailable
    When the S3 client retries (standard S3 retry behavior)
    Then DNS resolves to [node2, node3] (round-robin)
    And the retry succeeds on a healthy node

  @integration
  Scenario: Native client discovery updates after shard split
    Given the native client has cached discovery results
    And shard "shard-1" splits into "shard-1" and "shard-1b"
    When the native client's discovery cache TTL expires
    Then it re-queries discovery from a seed endpoint
    And receives the updated shard list including "shard-1b"
    And routes subsequent operations to the correct shard

  # --- Node lifecycle / operator drain workflow (ADR-035, spec-only) ---

  @integration
  Scenario: Operator workflow — graceful node retirement
    Given the cluster admin needs to retire node "n7" for hardware refresh
    And the cluster has 5 Active nodes [n1..n5] including n7
    When the operator runs `kiseki-admin node drain n7`
    Then the control plane validates that draining n7 leaves every shard with sufficient capacity for RF=3 (I-N4)
    And n7 transitions to Draining
    And progress is reported per shard (leadership transfers, learner adds, voter promotions)
    And on completion n7 transitions to Evicted
    And the operator is signalled completion with a per-shard summary
    And every state transition is recorded in the cluster audit shard (I-N6)

  @integration
  Scenario: Operator workflow — drain refused, replacement added, drain re-issued
    Given the cluster has exactly 3 Active nodes [n1, n2, n3]
    When the operator runs `kiseki-admin node drain n1`
    Then the request is refused with "insufficient capacity to maintain RF=3" (I-N4)
    And the operator adds a replacement node n4
    And the operator re-runs `kiseki-admin node drain n1`
    Then the drain is accepted and proceeds per the standard protocol
    And the audit log records both the refusal and the successful drain

  @integration
  Scenario: Operator workflow — drain cancellation
    Given node n1 is Draining with voter replacement in progress
    When the operator runs `kiseki-admin node drain-cancel n1`
    Then n1 transitions Draining → Active (I-N7)
    And the cancellation reason is recorded in the audit log
    And subsequent operations on n1 succeed normally

  # --- Dedup refcount access control (ADR-017) ---

  @unit
  Scenario: Cluster admin sees total refcount only
    Given chunk "abc123" is referenced by org-pharma (1 ref) and org-biotech (1 ref)
    And total refcount = 2
    When the cluster admin queries ChunkHealth for "abc123"
    Then the response includes total_refcount: 2
    And the response does NOT include per-tenant attribution
    And the cluster admin cannot determine which tenants share the chunk

  @unit
  Scenario: Dedup timing side channel — normalized write latency
    Given "org-pharma" writes plaintext P (new chunk, full write)
    And "org-biotech" writes the same plaintext P (dedup hit, refcount increment)
    When both write latencies are measured
    Then the dedup hit is NOT observably faster (optional: random delay normalizes timing)
    And an external observer cannot distinguish new-write from dedup-hit by timing

  # --- Workflow Advisory operational signals (ADR-020) ---
  # Operational observability covers the ADVISORY SUBSYSTEM itself from
  # the operator's perspective: its own health, its audit event flow,
  # and its side-by-side isolation from the data path. Client-facing
  # telemetry flows are spec'd in workflow-advisory.feature and the
  # per-context files.

  @unit
  Scenario: Advisory subsystem health reported to cluster admin
    Given the advisory subsystem is running on all storage nodes
    When the cluster admin queries operational metrics (per ADR-015)
    Then advisory-specific metrics are exposed, tenant-anonymized:
      | metric                           | cardinality        |
      | advisory_active_workflows_total  | cluster aggregate  |
      | advisory_hints_accepted_total    | cluster aggregate  |
      | advisory_hints_rejected_total    | by reason, aggregate |
      | advisory_hints_throttled_total   | cluster aggregate  |
      | advisory_channel_latency_p99_ms  | cluster aggregate  |
      | advisory_audit_write_rate        | cluster aggregate  |
      | advisory_state_by_scope          | enabled/draining/disabled counts |
    And workflow_id, phase_tag, and workload_id appear only as opaque hashes (I-A3, I-WA8)
    And no metric label has unbounded cardinality

  @unit
  Scenario: Advisory audit event volume and batching visible to operators
    Given the cluster sustains high advisory-hint traffic
    When the advisory audit emitter applies I-WA8 batching for hint-accepted and hint-throttled events
    Then the operator metric `advisory_audit_batching_ratio` exposes the ratio of batched:emitted events cluster-wide
    And per-tenant lifecycle events (declare, end, phase-advance, policy-violation) remain per-occurrence
    And the per-second per-(workflow_id, reason) sampling guarantee is visible in the audit shard

  @unit
  Scenario: Advisory audit growth triggers I-A5 safety valve if stalled
    Given advisory audit events on a tenant's audit shard have stalled (consumer behind by >24h)
    When the audit safety valve (I-A5) engages
    Then delta GC proceeds with a documented gap for that tenant
    And an operational alert is raised to cluster admin and tenant admin
    And the advisory subsystem continues to emit new events (rate-limited per I-WA8)

  @integration
  Scenario: Advisory subsystem isolation verified operationally
    Given synthetic load drives the advisory subsystem to 100% of its runtime capacity
    When data-path operations continue in parallel
    Then data-path p50 / p99 / p999 latencies remain within their published SLOs (I-WA2)
    And the operational metric `data_path_blocked_on_advisory_total` remains 0
    And if the metric ever rises above 0, a P0 alert fires and the advisory subsystem is candidate for circuit-break

  @integration
  Scenario: Advisory subsystem outage F-ADV-1 — operator-visible state
    Given the advisory subsystem on one node becomes unresponsive (F-ADV-1)
    When operational health checks run
    Then `advisory_health_status` for that node reports "unhealthy"
    And `data_path_health_status` for that node remains "healthy"
    And cluster admin is alerted to restart the advisory runtime
    And no tenant data-path operation records any failure attributable to this outage
