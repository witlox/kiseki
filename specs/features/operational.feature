Feature: Operational — Integrity monitoring, schema versioning, compression, observability
  Cross-cutting operational concerns that span multiple bounded contexts.

  Background:
    Given a Kiseki cluster with 5 storage nodes
    And tenant "org-pharma" with compliance tags [HIPAA, GDPR]
    And system key manager healthy at epoch 3

  # --- Runtime integrity monitor (ADR-018, I-O7) ---

  # @unit scenarios moved to crate-level unit tests:
  # "ptrace attachment detected" → kiseki-server/src/integrity.rs::ptrace_detection_status_variant
  # "Core dump attempt blocked" → kiseki-server/src/integrity.rs::core_dumps_blocked_status_variant
  # "Integrity monitor in development mode" → kiseki-server/src/integrity.rs::dev_mode_integrity_monitor_disabled

  # --- Schema versioning (ADR-004) ---

  @library
  Scenario: Rolling upgrade — mixed version cluster
    Given nodes [1, 2, 3] are running kiseki-server v1.0 (format version 1)
    When node 1 is upgraded to v1.1 (supports format versions [1, 2])
    Then node 1 reads format v1 deltas from other nodes
    And node 1 writes format v1 deltas (not v2, until all nodes upgraded)
    And Raft replication works across mixed versions
    And after all nodes upgraded: writers switch to format v2

  # --- Compression (I-K14) ---

  # @unit scenarios moved to crate-level unit tests:
  # "Tenant opts in to compression" → kiseki-crypto/src/compress.rs::tenant_opt_in_compression_4kb_padding
  # "Compressed chunk round-trip" → kiseki-crypto/src/compress.rs::compressed_chunk_roundtrip_large

  # --- Audit GC safety valve (ADR-009 revised, I-A5) ---

  # @unit scenarios moved to crate-level unit tests:
  # "Audit export stalls — safety valve" → kiseki-audit/src/event.rs::audit_gc_safety_valve_triggers
  # "Audit backpressure mode" → kiseki-audit/src/event.rs::audit_backpressure_throttles_when_enabled_and_behind
  # "Audit backpressure does not affect other tenants" → kiseki-audit/src/event.rs::audit_backpressure_tenant_scoped

  # --- Retention hold auto-creation (ADR-010) ---

  # @unit scenarios moved to crate-level unit tests:
  # "HIPAA namespace auto-creates retention hold" → kiseki-audit/src/event.rs::hipaa_auto_retention_hold
  # "Crypto-shred with force override" → kiseki-audit/src/event.rs::crypto_shred_force_override_audited

  # --- Crypto-shred invalidation broadcast (ADR-011, I-K15) ---

  @library
  Scenario: Crypto-shred triggers invalidation broadcast
    Given gateways [gw-1, gw-2] and stream processors [sp-1, sp-2] cache "org-pharma" KEK
    When crypto-shred is executed for "org-pharma"
    Then an invalidation broadcast is sent to [gw-1, gw-2, sp-1, sp-2]
    And components receiving the broadcast immediately purge cached KEK
    And crypto-shred returns success after KEK destruction + broadcast
    And it does NOT wait for all acknowledgments

  @library
  Scenario: Unreachable component — TTL expires naturally
    Given native client "client-1" on an unreachable compute node caches "org-pharma" KEK
    And the cache TTL is 60 seconds
    When crypto-shred is executed and invalidation broadcast sent
    And "client-1" does not receive the broadcast
    Then "client-1" can still decrypt data for up to 60 seconds
    And after 60 seconds, the cached KEK expires
    And subsequent operations from "client-1" fail with "key unavailable"

  # --- Writable mmap (ADR-013, I-O8) ---

  # @unit scenario "Writable shared mmap returns ENOTSUP" → kiseki-client/src/fuse_fs.rs::writable_mmap_returns_enotsup
  #   + kiseki-audit/src/event.rs::writable_mmap_enotsup_value

  # --- Multi-endpoint client resilience (ADR-019, I-O9) ---

  @library
  Scenario: NFS client reconnects after node failure
    Given an NFS client is connected to gateway on node 1
    And the NFS mount is configured with multiple server addresses [node1, node2, node3]
    When node 1 crashes
    Then the NFS client detects connection loss
    And reconnects to node 2 or node 3 automatically
    And NFS operations resume (session state re-established)

  @library
  Scenario: S3 client retries to different endpoint on error
    Given an S3 client sends PutObject to node 1
    And node 1 returns 503 Service Unavailable
    When the S3 client retries (standard S3 retry behavior)
    Then DNS resolves to [node2, node3] (round-robin)
    And the retry succeeds on a healthy node

  @library
  Scenario: Native client discovery updates after shard split
    Given the native client has cached discovery results
    And shard "shard-1" splits into "shard-1" and "shard-1b"
    When the native client's discovery cache TTL expires
    Then it re-queries discovery from a seed endpoint
    And receives the updated shard list including "shard-1b"
    And routes subsequent operations to the correct shard

  # --- Node lifecycle / operator drain workflow (ADR-035, spec-only) ---

  @library
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

  @library
  Scenario: Operator workflow — drain refused, replacement added, drain re-issued
    Given the cluster has exactly 3 Active nodes [n1, n2, n3]
    When the operator runs `kiseki-admin node drain n1`
    Then the request is refused with "insufficient capacity to maintain RF=3" (I-N4)
    And the operator adds a replacement node n4
    And the operator re-runs `kiseki-admin node drain n1`
    Then the drain is accepted and proceeds per the standard protocol
    And the audit log records both the refusal and the successful drain

  @library
  Scenario: Operator workflow — drain cancellation
    Given node n1 is Draining with voter replacement in progress
    When the operator runs `kiseki-admin node drain-cancel n1`
    Then n1 transitions Draining → Active (I-N7)
    And the cancellation reason is recorded in the audit log
    And subsequent operations on n1 succeed normally

  # --- Dedup refcount access control (ADR-017) ---

  # @unit scenario "Dedup timing side channel" → kiseki-audit/src/event.rs::dedup_timing_normalization

  # --- Workflow Advisory operational signals (ADR-020) ---

  # @unit scenarios moved to crate-level unit tests:
  # "Advisory subsystem health" → kiseki-audit/src/event.rs::advisory_health_metrics_shape
  # "Advisory audit batching" → kiseki-audit/src/event.rs::advisory_audit_batching_ratio
  # "Advisory audit growth triggers safety valve" → kiseki-audit/src/event.rs::advisory_audit_growth_triggers_safety_valve

  @library
  Scenario: Advisory subsystem isolation verified operationally
    Given synthetic load drives the advisory subsystem to 100% of its runtime capacity
    When data-path operations continue in parallel
    Then data-path p50 / p99 / p999 latencies remain within their published SLOs (I-WA2)
    And the operational metric `data_path_blocked_on_advisory_total` remains 0
    And if the metric ever rises above 0, a P0 alert fires and the advisory subsystem is candidate for circuit-break

  @library
  Scenario: Advisory subsystem outage F-ADV-1 — operator-visible state
    Given the advisory subsystem on one node becomes unresponsive (F-ADV-1)
    When operational health checks run
    Then `advisory_health_status` for that node reports "unhealthy"
    And `data_path_health_status` for that node remains "healthy"
    And cluster admin is alerted to restart the advisory runtime
    And no tenant data-path operation records any failure attributable to this outage
