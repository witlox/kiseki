Feature: Native Gateway Data Service — gRPC data-plane for native clients
  The native gateway data service exposes the data-plane operations
  (Read/Write/Delete/Lookup/streaming variants) directly via tonic on
  the data-path port (KISEKI_DATA_ADDR). Native clients (kiseki-client
  Rust SDK, eventually FUSE, Python, C/C++ FFI) reach the gateway
  without the S3 HTTP / NFS RPC / FUSE kernel hop.

  Two parallel verb families share the same service:
  - Object-flavored verbs operate on composition_id / name with
    commit-on-close semantics.
  - POSIX-flavored verbs operate on inode handles with
    partial-visible-on-fsync semantics, on a strict superset of the
    FUSE POSIX subset (ADR-013).

  Background:
    Given a Kiseki cluster with tenant "org-pharma"
    And tenant "org-pharma" has a client mTLS cert with SAN URI "spiffe://kiseki/tenant/org-pharma"
    And namespace "trials" registered in tenant "org-pharma"
    And the cluster's data-path gRPC port serves GatewayDataService
    And native client "client-a" is configured with the tenant cert and the cluster's fabric discovery seed addresses

  # --- Authentication and tenant binding (I-NG1) ---

  @native @auth
  Scenario: Native PUT — cert SAN matches payload tenant_id
    When client-a sends a native Write with payload tenant_id="org-pharma" and namespace_id="trials"
    Then the proto-handler validates the SAN URI carries "spiffe://kiseki/tenant/org-pharma"
    And the SAN-derived tenant matches the payload tenant_id
    And the request proceeds to the gateway
    And the write completes successfully

  @native @auth
  Scenario: Native PUT — cert SAN does NOT match payload tenant_id
    Given client-a's cert SAN URI is "spiffe://kiseki/tenant/org-pharma"
    When client-a sends a native Write with payload tenant_id="org-bank"
    Then the proto-handler rejects the request with PermissionDenied at the boundary
    And no gateway work runs (no audit event for the gateway op, but a security-failure audit event IS emitted)
    And the rejection happens before any composition or chunk lookup

  # --- Object-flavored writes (I-NG2) ---

  @native @objects
  Scenario: Native object PUT — small payload (≤ inline threshold)
    Given the inline threshold is 8 KiB
    When client-a sends a unary Write of 4 KiB to namespace "trials" with idempotency_key="put-001"
    Then the request is a single unary gRPC call (no streaming)
    And the server returns Ok with the new composition_id
    And a follow-up native Read returns the same 4 KiB

  @native @objects
  Scenario: Native object PUT — streaming payload, commit-on-close
    Given the inline threshold is 8 KiB and per-stream cap is 64 MiB
    When client-a opens a streaming Write of 16 MiB
    And streams the 16 MiB across multiple gRPC frames
    And calls CommitStream
    Then the server returns Ok with the new composition_id
    And readers can fetch the object only after CommitStream returned Ok
    And no reader observed any partial state during the stream

  @native @objects
  Scenario: Native object PUT — stream interrupted before CommitStream
    When client-a opens a streaming Write of 16 MiB
    And streams 8 MiB
    And the connection drops before CommitStream
    Then the partial state is never visible to any reader (I-NG2)
    And the server reclaims the partial state within the idempotency-key dedup window (5 min)
    When client-a retries with the same idempotency_key
    Then the server treats it as a fresh write, not a duplicate

  @native @objects
  Scenario: Native object PUT — retry with the same idempotency_key
    When client-a writes 4 KiB to "trials/checkpoint.pt" with idempotency_key="ck-42" and the response is lost in transit
    And client-a retries with the same idempotency_key="ck-42" within 5 minutes
    Then the server recognizes the duplicate
    And returns the original composition_id, not a new one
    And the chunk store sees only one underlying write

  # --- POSIX-flavored writes (I-NG3) ---

  @native @posix
  Scenario: Native POSIX write — partial-visible-on-fsync
    When client-a opens inode for "trials/run.log" in Write mode
    And writes 4 KiB at offset 0
    And writes 4 KiB at offset 4096
    Then a concurrent reader opening the same inode does NOT yet see the 8 KiB
    When client-a calls Fsync on the inode
    Then the concurrent reader's next Read sees the 8 KiB (matches POSIX fsync(2) semantics)

  @native @posix
  Scenario: Native POSIX rename within shard — atomic
    Given namespace "trials" maps to shard S1 covering both "src/" and "dst/" directories
    When client-a calls RenameWithinShard from "src/file" to "dst/file"
    Then the rename commits as a single delta on shard S1
    And no reader observes a state where neither name exists
    And no reader observes a state where both names exist

  @native @posix
  Scenario: Native POSIX rename across shards — EXDEV
    Given "src/" maps to shard S1 and "dst/" maps to shard S2
    When client-a calls RenameWithinShard from "src/file" to "dst/file"
    Then the server returns EXDEV (I-NG4 honors I-L8)
    And no atomic cross-shard rename is attempted

  @native @posix
  Scenario: Native POSIX lease-based RMW — exclusive write
    When client-a calls AcquireLease(inode="run.log", mode=Write)
    Then the server returns a lease with TTL=30s
    And client-a writes locally without per-op coordination
    When client-b calls AcquireLease(inode="run.log", mode=Write)
    Then the server returns LeaseHeld with the lease holder identity and ttl_remaining_ms
    When client-a calls ReleaseLease
    Then a subsequent AcquireLease from client-b succeeds

  @native @posix
  Scenario: Native POSIX lease — holder dies, lease expires
    Given client-a holds a Write lease on inode "run.log" with the configured lease TTL
    And client-a stops sending RenewLease
    When the configured lease TTL plus the renewal grace window elapses
    Then the lease expires server-side
    And uncommitted writes on the lease are invalidated
    And client-b's next AcquireLease succeeds with a fresh fencing token

  @native @posix
  Scenario: Native POSIX lease — partition-heal split-brain rejected by fencing
    Given client-a holds a Write lease on inode "run.log" with fencing_token=42
    And a network partition isolates client-a from the cluster
    And the lease expires server-side after the configured TTL
    And client-b acquires the lease with fencing_token=43
    When the partition heals and client-a (still believing it holds the lease) issues a Write with fencing_token=42
    Then the server rejects the write with LeaseFenced{current=43} (I-NG12)
    And client-a's audit event records the fenced write attempt
    And no data was written under the stale token

  # --- Hybrid leader routing (I-NG8) ---

  @native @routing
  Scenario: Native client dials shard leader directly (steady state)
    Given client-a's topology cache identifies shard S1's leader as node-2
    When client-a sends a Write to namespace "trials" routed to shard S1
    Then client-a opens a connection to node-2 directly
    And the request commits in one hop (no proxy)

  @native @routing
  Scenario: Native client receives NotLeader and refreshes topology
    Given client-a's topology cache says shard S1's leader is node-2
    But the leader has actually migrated to node-3
    And node-2 is configured with the proxy-fallback path disabled (client-side discovery only)
    When client-a sends a Write to node-2
    Then node-2 responds with NotLeader{leader=node-3} (one round-trip cost paid by the client)
    And client-a's topology cache is refreshed to identify node-3 as leader
    And client-a re-issues the Write to node-3
    And the Write commits successfully

  @native @routing
  Scenario: Native client transparently uses server-side proxy fallback
    Given client-a's topology cache says shard S1's leader is node-2
    But the leader has actually migrated to node-3
    And node-2 is configured with the proxy-fallback path enabled
    When client-a sends a Write to node-2
    Then node-2 proxies the request in-process to node-3
    And node-2 returns the write outcome to client-a in a single round-trip
    And the trailing metadata carries the new topology_version
    And client-a's topology cache is refreshed to identify node-3 as leader via the topology_version mismatch path (I-NG13)

  @native @routing
  Scenario: Server-side proxy fallback — proxying node fails mid-proxy
    Given client-a issues a Write to node-2 which is acting as a proxy to leader node-3
    When node-2 crashes between (a) committing the proxied request on node-3 and (b) returning the response to client-a
    Then client-a's RPC fails with Aborted{proxy_failure}
    When client-a refreshes topology, dials node-3 directly, and retries with the same idempotency_key
    Then the server returns the original outcome (idempotency dedup via I-NG5 + A-NG10) and the write commits exactly once

  # --- Streaming boundary (I-NG9) ---

  @native @streaming
  Scenario: Native Write — payload above per-stream cap goes multipart
    Given the per-stream cap is 64 MiB
    When client-a needs to write 200 MiB
    Then the client opens a multipart session with InitMultipart
    And uploads 4 parts via PutPart (50 MiB each)
    And finalizes via CompleteMultipart
    And the object becomes visible only after CompleteMultipart returns Ok

  # --- Encryption boundary (I-NG6) ---

  @native @encryption
  Scenario: Native Read — server-decrypt mode (default)
    Given namespace "trials" has crypto_boundary = ServerOnly
    When client-a sends a native Read for composition_id="c-1"
    Then the server decrypts the chunk in-process
    And the response carries plaintext over the mTLS channel

  @native @encryption @trusted-compute
  Scenario: Native Read — client-decrypt mode (trusted compute pool)
    Given namespace "trials-gpu" has crypto_boundary = TrustedCompute
    When client-a sends a native Read for composition_id="c-2"
    Then the server returns the sealed envelope and the DEK reference
    And client-a fetches the DEK from the keymanager
    And client-a decrypts the envelope locally
    And the wire never carried plaintext for this read

  # --- Audit (I-NG7) ---

  @native @audit
  Scenario: Native data op — audit event uses cert SAN as principal
    When client-a sends a native Write to namespace "trials"
    Then the audit pipeline emits an event with principal="spiffe://kiseki/tenant/org-pharma" tenant_id="org-pharma" namespace_id="trials" workflow_ref=<the workflow_ref carried in the request>
    And the event is shape-identical to S3 / NFS / FUSE audit events for the same logical op

  # --- Performance witness (A-NG11) ---

  @native @perf @smoke
  Scenario: Native object 64 KiB GET — per-node throughput target
    Given a single-node cluster with the native gateway data service
    And the in-process gateway floor measurement (graduation gate, A-NG11) sustained 114 995 op/s on this hardware (≥100 000 threshold cleared 2026-05-05)
    When kiseki-profile drives 16 concurrent clients issuing 64 KiB native Reads against pre-warmed objects for 30 seconds
    Then the sustained throughput is at least 80 000 op/s
    And the p99 latency is below 10 ms
    And the error rate is below 0.01% (≤ ~1 in 10 000 ops)

  @native @perf @smoke
  Scenario: Native object 64 KiB PUT — per-node throughput target
    Given a single-node cluster with the native gateway data service
    And the in-process gateway floor for 64 KiB PUT was measured at 20 089 op/s on this hardware (the gateway-internal write path is the binder, not the protocol layer)
    When kiseki-profile drives 16 concurrent clients issuing 64 KiB native Writes for 30 seconds
    Then the sustained throughput is at least 14 000 op/s (the in-process PUT floor × 0.7 gRPC tax)
    And the p99 latency is below 10 ms
    And the error rate is below 0.01% (≤ ~1 in 10 000 ops)
    # Higher PUT throughput requires gateway-internal-write-path
    # work outside ADR-042's scope (composition store hot path,
    # AEAD seal cost, chunk-id HMAC). Tracked as a follow-up.

  # --- F-H3 SAN URI canonicalization near-miss (I-NG1) ---

  @native @auth
  Scenario Outline: Native PUT — SAN URI near-miss is rejected
    Given client-a's cert SAN URI is "<actual_san>"
    When client-a sends a native Write with payload tenant_id="<payload_tenant>"
    Then the proto-handler rejects the request with PermissionDenied{san_canonicalization_mismatch}

    Examples:
      | actual_san                                     | payload_tenant     | rejection_reason             |
      | spiffe://kiseki/tenant/org-pharma/             | org-pharma         | trailing slash               |
      | SPIFFE://kiseki/tenant/org-pharma              | org-pharma         | scheme not lowercased        |
      | spiffe://Kiseki/tenant/org-pharma              | org-pharma         | authority not lowercased     |
      | spiffe://кiseki/tenant/org-pharma              | org-pharma         | non-ASCII (Cyrillic к)       |
      | spiffe://kiseki/tenant/org%2Dpharma            | org-pharma         | percent-encoded unreserved   |
      | spiffe://kiseki/tenant/org-pharma              | org-pharma/        | payload tenant trailing slash|

  # --- I-NG6c TrustedCompute requires crypto_shred = best_effort ---

  @native @encryption @config
  Scenario: TrustedCompute requested without best-effort shred — rejected
    Given namespace "trials-gpu" exists with crypto_boundary=ServerOnly
    When the operator calls UpdateNamespace setting crypto_boundary=TrustedCompute (and leaves crypto_shred_policy at its default "enforced")
    Then the server rejects the update with InvalidArgument{crypto_shred_policy_required}
    And the namespace remains in ServerOnly mode

  @native @encryption @config
  Scenario: TrustedCompute with explicit best-effort shred — accepted
    Given namespace "trials-gpu" exists with crypto_boundary=ServerOnly
    When the operator calls UpdateNamespace setting BOTH crypto_boundary=TrustedCompute AND crypto_shred_policy=best_effort
    Then the server accepts the update
    And subsequent native reads of trials-gpu return sealed envelopes and DEK references (client-decrypt mode active)
    And the namespace metadata reflects both flags so tenants and auditors can detect the residual exposure window

  # --- I-NG11 concurrent stream cap ---

  @native @resource-limits
  Scenario: Per-tenant concurrent stream cap enforced at proto boundary
    Given the per-tenant concurrent-stream cap for "org-pharma" is 256
    And client-a already has 256 in-flight streaming Writes open
    When client-a issues a 257th OpenStream
    Then the server rejects with ResourceExhausted{native_concurrent_stream_cap} BEFORE allocating any server-side staging buffer (I-NG11)
    And the rejection emits a security-failure audit event (I-NG7)
    When one of the in-flight streams completes via CommitStream
    Then the next OpenStream succeeds (cap counter decremented)

  # --- I-NG12 fencing token threaded through audit ---

  @native @posix @audit
  Scenario: Fencing token recorded in audit event
    Given client-a holds a Write lease on inode "run.log" with fencing_token=99
    When client-a writes 4 KiB under the lease
    Then the audit event for the write records lease_fencing_token=99 alongside the principal and workflow_ref

  # --- I-NG13 topology_version push-based invalidation ---

  @native @routing
  Scenario: Topology cache refreshed on topology_version mismatch
    Given client-a's cached topology_version is 100
    And the cluster topology_version has advanced to 101 due to a leader change for shard S2
    When client-a sends a native Read whose response trailing metadata carries topology_version=101
    Then client-a refreshes its topology cache before the next Write (push-based, no waiting for the 30 s TTL)

  # --- I-NG14 lease + drain interaction ---

  @native @posix @drain
  Scenario: Drain waits for outstanding leases to expire
    Given node-2 hosts the leader for shard S1
    And client-a holds a Write lease on an inode in S1 with TTL=30s
    When the operator initiates Drain on node-2
    Then the drain protocol does NOT forcibly revoke client-a's lease
    And the drain progress reports "waiting for outstanding leases"
    When client-a's lease expires (or is voluntarily released)
    Then the drain proceeds: leadership is transferred off node-2 and a replacement voter is added (per ADR-035)

  @native @posix @drain
  Scenario: New AcquireLease against a draining node — rejected
    Given node-2 hosts the leader for shard S1 and is in Draining state
    And the drain quiesce window remaining is 10 seconds
    When client-b calls AcquireLease(inode in S1, mode=Write, requested_ttl=30s)
    Then the server rejects with Unavailable{node_draining} because the requested TTL would outlast the quiesce window (I-NG14)

  # --- F-NG8 cert revocation mid-session (A-NG17) ---

  @native @auth
  Scenario: Long-running stream torn down on cert revocation
    Given client-a is in the middle of a 60 MiB streaming Write (stream open for 45 seconds, halfway through)
    And the cluster's CRL is updated to revoke client-a's cert
    When the server's periodic cert re-validation runs (default 60 s)
    Then the server tears down the stream with Unauthenticated{reason=cert_revoked}
    And the partial stream state is reclaimed within the idempotency-key dedup window
    And no commit-on-close visibility leak occurs

  # --- A-NG18 clock skew bound ---

  @native @posix @clock
  Scenario: Clock skew within tolerance — leases work
    Given client-a's clock is 4 seconds ahead of the cluster's clocks (within the 5-second I-T1/I-T2 / A-NG18 tolerance)
    When client-a acquires a 30-second lease and renews at +25 seconds by client clock
    Then the server processes the renewal at +21 seconds by server clock and accepts (lease still alive)
    And no spurious lease expiry occurs

  @native @posix @clock @observability
  Scenario: Clock skew exceeds tolerance — alarm raised
    Given client-a's clock is 30 seconds ahead of the cluster's clocks
    When client-a issues any native op
    Then the metric kiseki_clock_skew_seconds exceeds the alarm threshold (5s) and an operator alert fires
    And the op itself proceeds (correctness preserved by the existing time invariants and clock-quality observability)

  # --- ADR-042 round-1 + round-2 + round-3 binding-rewrite scenarios ---
  # Closes the gate-1 fault-injection BDD coverage gap (R3-O3) and
  # asserts the contract/binding architecture's runtime behavior.

  @native @binding-probe
  Scenario: Binding probe timeout falls back to next-best binding
    Given the kiseki-server is configured with KISEKI_NATIVE_PROBE_TIMEOUT_MS=10
    And the host has libibverbs installed but `/sys/class/infiniband/*` is artificially blocked
    When the server starts and runs phase-1 probes
    Then the ibverbs binding self-disqualifies with Unavailable{reason="no usable port"}
    And the startup banner enumerates: tcp-framed (Available), grpc-h2 (Available), ibverbs (Unavailable), libfabric (per host)
    And the server starts successfully with at least one binding listening
    And kiseki_native_binding_probe_duration_seconds{binding="ibverbs"} records the probe time

  @native @binding-probe
  Scenario: All bindings fail probe — server exits cleanly
    Given the kiseki-server is started with no listen addresses configured for any binding
    When the server runs phase-3 listener-spawn
    Then the server exits with code 3 and the message indicates no bindings could spawn

  @native @binding-restart
  Scenario: Binding listener crashes mid-flight — clients drain gracefully
    Given a healthy native client with open connections on tcp-framed and grpc-h2 to node-2
    And client-a has 3 in-flight requests on the tcp-framed connection
    When the tcp-framed listener on node-2 panics
    Then the runtime emits kiseki_native_binding_listener_crashed_total{binding="tcp-framed"} and bumps topology_version
    And the client observes the topology change on the next response trailer
    And the client opens a new grpc-h2 connection to node-2 for new work
    And the 3 in-flight tcp-framed requests run to completion within KISEKI_NATIVE_DRAIN_BUDGET_MS
    And kiseki_native_client_binding_drain_total{binding="tcp-framed", reason="listener_crashed"} increments by 1

  @native @binding-restart
  Scenario: Backoff-restart restores binding after crash
    Given the tcp-framed binding crashed and entered backoff
    When the runtime's backoff-restart timer fires (default 5 s)
    And the listener spawn succeeds
    Then topology_version bumps and the new endpoint is advertised
    And clients eventually re-establish tcp-framed connections to node-2

  @native @topology
  Scenario: Topology version regress falls back to TTL safety net
    Given the cluster manually publishes a regressed topology_version (operator error simulation)
    When the client polls or sees the regressed version on a response trailer
    Then the client refuses the regression and continues with its highest-seen version
    And after 30 s the TopologyCache TTL fires and the client refreshes regardless

  @native @routing
  Scenario: Per-edge selection — heterogeneous binding cluster
    Given a 4-node cluster where node-1 + node-2 advertise libfabric/cxi + tcp-framed + grpc-h2
    And node-3 + node-4 advertise tcp-framed + grpc-h2 only
    And the local client environment has libfabric/cxi available
    When the client opens connections to all four nodes for a multi-node operation
    Then the client uses libfabric/cxi for node-1 + node-2
    And the client uses tcp-framed for node-3 + node-4 (next-best mutually-supported)
    And request_id + idempotency_key carry across the binding boundary within the same operation

  @native @drain
  Scenario: Draining node serves in-flight lease writes but rejects new opens
    Given node-2 is currently the leader for shard S1
    And node-2 is in Draining state with drain_state.accepts_new_work=false (quiesce in progress)
    And client-a holds an active lease on inode I in S1
    When client-a issues a lease-bound Write to node-2 (carrying a valid fencing_token)
    Then the server processes the write to completion (in-flight lease respected)
    When client-b calls AcquireLease(inode J in S1, mode=Write)
    Then the server rejects with Unavailable{node_draining}

  @native @drain
  Scenario: Graceful release window allows straggler in-flight work
    Given node-2 has quiesced (drain_state.accepts_new_work flips false → true for the graceful-release window)
    And client-c has a stale topology that still routes to node-2
    When client-c issues a lease-bound write within KISEKI_NATIVE_DRAIN_GRACEFUL_RELEASE_MS
    Then the server accepts the write (briefly) until the window closes
    And after the window, node-2 transitions to Evicted and bindings advertise empty

  @native @binding-cxi @attestation
  Scenario: cxi attestation envelope replay rejected
    Given the libfabric/cxi binding is active on node-1
    And client-a has successfully attested with envelope E (nonce N, issued_at T)
    When the same envelope E is replayed against node-1 within 60 seconds
    Then the server rejects with Unauthenticated{cxi_attestation_replay}
    And the metric kiseki_native_binding_handshake_failures_total{binding="Libfabric/Cxi", reason="cxi_attestation_replay"} increments

  @native @binding-cxi @attestation
  Scenario: cxi attestation rate-limited under flood
    Given the libfabric/cxi binding is active on node-1
    And source S has consumed its rate-limit budget (default 100 attempts / 60 s)
    When source S issues a 101st attestation attempt within the window
    Then the server rejects with ResourceExhausted{cxi_attestation_rate_limit}
    And kiseki_native_cxi_attestation_throttled_total{source="S", reason="rate_limit"} increments
    And the rejection happens before envelope decode (no ECDSA verify cost paid)

  @native @binding-cxi @attestation
  Scenario: cxi attestation oversize envelope rejected
    Given the libfabric/cxi binding is active on node-1
    When a client sends a CxiAttestationEnvelope with cert_chain_der totalling 9 KiB (over the 8 KiB cap)
    Then the server rejects with InvalidArgument{cxi_attestation_oversize}
    And the connection closes immediately
    And no postcard parsing of the envelope body is attempted
