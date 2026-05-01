Feature: pNFS Flexible Files Layout (ADR-038, RFC 8435)

  Real Linux pNFS clients receive a Flexible Files Layout (RFC 8435)
  from the MDS, resolve `device_id` via GETDEVICEINFO, and issue
  READ/WRITE/COMMIT directly to a per-storage-node Data Server (DS)
  endpoint. The DS is stateless: it validates the self-authenticating
  fh4 (HMAC-SHA256/16 over tenant‖namespace‖composition‖stripe‖expiry,
  with domain-separation tag `kiseki/pnfs-fh/v1\0`) and forwards the
  op to the existing `GatewayOps`.

  Layout state is MDS-authoritative; recovery uses standard NFSv4.1
  session reclaim. LAYOUTRECALL is fired from drain (ADR-035), shard
  split/merge (ADR-033/034), composition deletion, and fh4 MAC key
  rotation via the control-plane TopologyEventBus (§D10). The 5-min
  layout TTL (auto-halved to 60s in plaintext fallback mode) is the
  ultimate safety net.

  Background:
    Given a 3-node pNFS cluster
    And a bootstrap namespace "default" with tenant "org-test"
    And `K_layout` is derived from the master key

  # ============================================================================
  # Phase 15a — DS surface
  # Establishes the new ds_addr listener (default :2052), fh4 MAC
  # validation, the strict op subset, and the dual-flag plaintext fallback.
  # ============================================================================

  @library @pnfs-15a
  Scenario: Valid fh4 + READ on the DS returns plaintext from GatewayOps
    Given a composition "obj-1" with 64 KiB of data exists in "default"
    And the MDS has issued a layout for "obj-1" stripe 0
    When a client sends NFSv4.1 READ to the DS using stripe-0 fh4 with offset 0 length 4096
    Then the DS returns NFS4_OK with 4096 bytes of plaintext
    And the bytes match the expected slice of the composition
    And the DS held no per-fh4 state across the call

  @library @pnfs-15a
  Scenario: DS rejects forged fh4 with NFS4ERR_BADHANDLE
    Given a fh4 whose MAC was computed with a different `K_layout`
    When a client sends READ to the DS using that fh4
    Then the DS returns NFS4ERR_BADHANDLE
    And the constant-time MAC compare flagged a mismatch
    And no GatewayOps::read call was made

  @library @pnfs-15a
  Scenario: DS rejects expired fh4 with NFS4ERR_BADHANDLE
    Given a fh4 whose `expiry_ms` is 1 second in the past
    When a client sends READ to the DS using that fh4
    Then the DS returns NFS4ERR_BADHANDLE
    And no GatewayOps::read call was made

  @library @pnfs-15a
  Scenario: DS rejects ops outside the allowed subset with NFS4ERR_NOTSUPP
    Given a valid fh4 for "obj-1" stripe 0
    When a client sends a COMPOUND containing PUTFH then ALLOCATE
    Then the DS returns NFS4ERR_NOTSUPP for ALLOCATE
    And the COMPOUND aborts on the first error
    And no later op in the COMPOUND was parsed

  @library @pnfs-15a
  Scenario: DS accepts only the eight required ops
    When the DS dispatcher table is enumerated
    Then exactly eight op codes are handled: EXCHANGE_ID, CREATE_SESSION, DESTROY_SESSION, PUTFH, READ, WRITE, COMMIT, GETATTR
    And every other op returns NFS4ERR_NOTSUPP

  @library @pnfs-15a
  Scenario: TLS-wrapped DS listener uses the cluster TlsConfig
    Given a cluster TLS bundle (CA, cert, key) is loaded
    When the DS listener is started on `:2052`
    Then the listener wraps `TcpListener` with `TlsConfig::server_config`
    And a non-TLS handshake is rejected at the transport layer

  @library @pnfs-15a
  Scenario: TLS-wrapped MDS listener uses the cluster TlsConfig
    Given a cluster TLS bundle is loaded
    When the MDS NFS listener is started on `nfs_addr`
    Then the listener wraps `TcpListener` with `TlsConfig::server_config`

  @library @pnfs-15a
  Scenario: Plaintext fallback refused when only the env var is set
    Given `KISEKI_INSECURE_NFS=true` but `[security].allow_plaintext_nfs=false`
    When the server boots
    Then the server refuses to start with a "plaintext NFS requires both flags" error

  @library @pnfs-15a
  Scenario: Plaintext fallback refused when only the config flag is set
    Given `[security].allow_plaintext_nfs=true` but `KISEKI_INSECURE_NFS` is unset
    When the server boots
    Then the server refuses to start with a "plaintext NFS requires both flags" error

  @library @pnfs-15a
  Scenario: Plaintext fallback enabled with both flags emits audit event each boot
    Given both `[security].allow_plaintext_nfs=true` and `KISEKI_INSECURE_NFS=true`
    And the served namespace has exactly one tenant
    When the server boots
    Then a `SecurityDowngradeEnabled{reason="plaintext_nfs"}` audit event is emitted
    And the startup log records the WARN banner described in ADR-038 §D4.2
    And the effective `layout_ttl_seconds` is 60
    And the NFS listener accepts plaintext TCP connections

  @library @pnfs-15a
  Scenario: Plaintext fallback refused when more than one tenant is served
    Given both plaintext flags are set
    And the namespace map has 2 tenants on the same listener
    When the server boots
    Then the server refuses to start with a "plaintext NFS is single-tenant only" error

  @library @pnfs-15a
  Scenario: DS is stateless — kill-and-restart resumes via fresh fh4 retry
    Given a composition "obj-2" exists and a client has a valid fh4
    When the DS task is killed mid-flight
    And the DS task is restarted
    And the client retries the same op with the same fh4
    Then the op succeeds with the same result as before the restart
    And no DS-side recovery state was inspected

  # ============================================================================
  # Phase 15b — MDS layout wire-up
  # Wires LAYOUTGET to produce a real RFC 8435 ff_layout4 body and
  # adds GETDEVICEINFO. Layout cache gets explicit eviction (I-PN8).
  # ============================================================================

  # Phase 15c.9: layout shape changed from "N segments × 1 mirror"
  # to "1 segment × N mirrors" (RFC 8435 §13.2 striping inside one
  # segment via `ffl_mirrors<>` + `stripe_unit`). The kernel
  # picks `mirror_idx = (offset / stripe_unit) % num_mirrors` and
  # dispatches each read to the chosen mirror's DS at the absolute
  # composition offset. Replication-3 means every node has every
  # byte, so the mirror index is purely load-balancing.
  @library @pnfs-15b
  Scenario: LAYOUTGET returns a well-formed ff_layout4 body
    Given a composition "obj-3" of 4 MiB exists in "default"
    When the client sends LAYOUTGET for "obj-3" range [0, 4 MiB)
    Then the response is a well-formed `ff_layout4` per RFC 8435 §5.1
    And it contains 3 mirrors covering the 4 MiB segment
    And each mirror carries a 76-byte fh4 (60-byte payload + 16-byte MAC)
    And consecutive mirrors are assigned to distinct storage nodes (one per cluster node)

  @library @pnfs-15b
  Scenario: GETDEVICEINFO resolves device_id to reachable DS addresses
    Given a layout for "obj-3" was issued referencing 3 device_ids
    When the client sends GETDEVICEINFO for each device_id
    Then each response is a `ff_device_addr4` per RFC 8435 §5.2
    And every `netaddr4` resolves to one of the 3 storage nodes' `ds_addr`
    And the `versions` field lists exactly `[NFSv4_1]`

  @library @pnfs-15b
  Scenario: Layout cache sweeper evicts entries past TTL
    Given the layout cache TTL is set to 200 ms for the test
    And 100 LAYOUTGETs have been issued
    When 250 ms elapse and the sweeper runs
    Then the layout cache is empty
    And no LAYOUTRECALL was fired (TTL eviction is silent per I-PN8)

  @library @pnfs-15b
  Scenario: Layout cache LRU-evicts on capacity overflow
    Given `layout_cache_max_entries=4`
    When 6 LAYOUTGETs are issued for distinct compositions
    Then exactly 4 entries are live
    And the 2 evicted entries are the 2 with the smallest `issued_at_ms`

  # Deferred to the python e2e harness (tests/e2e/test_pnfs.py) —
  # mounting a real Linux 6.7+ pNFS client requires a privileged
  # docker container joined to the cluster's docker network, which
  # the in-process BDD runner can't provide. The `@e2e-deferred`
  # tag is filtered out by `tests/acceptance.rs::main` so the
  # scenario shows as skipped here and gets exercised by
  # `pytest tests/e2e/test_pnfs.py::test_pnfs_plaintext_fallback`
  # (closed in Phase 15c.5 step 3).
  @library @pnfs-15b @e2e-deferred
  Scenario: Real Linux pNFS client round-trip (RFC fidelity)
    Given a Linux 6.7+ pNFS client is available with `xprtsec=mtls`
    When the client mounts the export and reads 1 MiB sequentially through one DS
    Then `/proc/self/mountstats` shows non-zero LAYOUTGET counters
    And shows non-zero per-DS READ counters
    And the bytes returned match the canonical composition

  # ============================================================================
  # Phase 15d — TopologyEventBus
  # Resolves ADV-038-3 / ADV-038-8: drain, split, merge, composition
  # delete, and key rotation events become subscribable from the gateway.
  # ============================================================================

  @library @pnfs-15d
  Scenario: Drain commit emits exactly one NodeDraining event
    Given a TopologyEventBus subscriber is attached
    When the drain orchestrator commits a state transition to `Draining` for node "n1"
    Then exactly one `NodeDraining{node_id=n1}` event is observed on the bus
    And the event was emitted AFTER the control-Raft commit

  @library @pnfs-15d
  Scenario: Aborted drain emits no event
    Given a TopologyEventBus subscriber is attached
    When the drain orchestrator's pre-check refuses with InsufficientCapacity
    Then no `NodeDraining` event is observed on the bus

  @library @pnfs-15d
  Scenario: Shard split commit emits one ShardSplit event
    Given a TopologyEventBus subscriber is attached
    When a shard split commits in the namespace shard map
    Then exactly one `ShardSplit{parent, children}` event is observed
    And the event arrives after the shard-map Raft commit

  @library @pnfs-15d
  Scenario: Composition delete emits CompositionDeleted
    Given a composition "obj-4" exists
    And a TopologyEventBus subscriber is attached
    When the composition is deleted
    Then exactly one `CompositionDeleted{composition=obj-4}` event is observed

  @library @pnfs-15d
  Scenario: Subscriber lag is signaled rather than silently dropped
    Given a TopologyEventBus subscriber that processes one event per second
    When 2000 events are emitted in 100 ms (channel cap = 1024)
    Then the subscriber observes at least one `Lag(n)` indication
    And the `pnfs_topology_event_lag_total` Prometheus counter has incremented

  # ============================================================================
  # Phase 15c — LAYOUTRECALL + integration
  # Bus subscribers in the gateway invalidate and recall in response.
  # I-PN5 SLA: ≤ 1 sec from event commit to recall send-out.
  # ============================================================================

  @library @pnfs-15c
  Scenario: Drain triggers LAYOUTRECALL within 1 second
    Given a layout has been issued referencing node "n1" as a DS
    When the drain orchestrator commits drain on "n1"
    Then a LAYOUTRECALL is sent to the holding client within 1 second
    And subsequent client reads with the recalled fh4 return NFS4ERR_BADHANDLE

  @library @pnfs-15c
  Scenario: Shard split triggers LAYOUTRECALL for affected layouts
    Given a layout was issued for a composition whose shard then splits
    When the split commits
    Then a LAYOUTRECALL is sent for the affected layouts within 1 second

  @library @pnfs-15c
  Scenario: Composition deletion triggers LAYOUTRECALL
    Given a layout was issued for "obj-5"
    When "obj-5" is deleted
    Then a LAYOUTRECALL is sent for that layout within 1 second
    And subsequent ops return NFS4ERR_STALE per RFC 8435 §6

  @library @pnfs-15c
  Scenario: fh4 MAC key rotation triggers bulk LAYOUTRECALL
    Given 5 layouts are outstanding across 2 compositions
    When `K_layout` is rotated
    Then LAYOUTRECALL fires for all 5 layouts within 1 second
    And subsequently re-issued layouts MAC-validate under the new key

  @library @pnfs-15c
  Scenario: TTL fallback works when subscriber is dead
    Given the LayoutManager subscriber task has been killed
    And a layout was issued with a 2-second TTL (test override)
    When 3 seconds elapse without any topology event delivery
    Then a subsequent DS op with that fh4 returns NFS4ERR_BADHANDLE
    And the layout cache contains 0 entries (sweeper)

  @library @pnfs-15c
  Scenario: Subscriber lag triggers full layout-cache flush (safety net)
    Given a layout is in the MDS cache
    When the subscriber observes a `Lag(n)` indication
    Then the layout cache is fully invalidated
    And a subsequent client op causes a fresh LAYOUTGET
    And `pnfs_topology_event_lag_total{reason="recv_lag"}` has incremented
