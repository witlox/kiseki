# BDD Depth Audit — 2026-04-26

## Executive summary

Successor to the 2026-04-25 audit (which preceded Phase 13f). This
report classifies every @integration scenario except the 14 already
covered by `specs/fidelity/phase-13f-audit.md` (link those ratings as
authoritative; do not duplicate).

**State at HEAD** `3b903a4`:
- 22 feature files, 241 @integration scenarios (181 fast + 60 slow).
- 19 step-definition files, ~2,700 step functions.
- Default test run is `cargo test -p kiseki-acceptance` (no
  `--features slow-tests`); filter at `acceptance.rs:773-775` skips
  any scenario tagged `@slow`. So 60 scenarios are silent unless the
  CI explicitly opts in.

**Headline depth distribution (227 audited)**:

| Depth | Count | Acceptable for @integration? |
|---|---:|---|
| THOROUGH | 35  | Yes |
| MOCK     | 92  | No (would be acceptable for @unit) |
| SHALLOW  | 81  | No |
| STUB     | 19  | No (use `todo!()` instead and mark as gap) |

Severity legend (used in tables below):
- **HIGH** — security or data-integrity invariant unenforced
- **MEDIUM** — functional invariant weakly enforced
- **LOW** — cosmetic / acceptable simplification

---

## Excluded from this audit

The 14 scenarios listed in `specs/fidelity/phase-13f-audit.md` (Phase
13f Audit, 2026-04-26) are **not re-rated here**. Treat that file's
ratings as the authoritative depth classification for those scenarios.

In summary, those 14 are:
- protocol-gateway.feature: NFSv4.1 state management, NFS gateway over
  TCP, S3 gateway over TCP, Gateway cannot reach KMS, Gateway cannot
  reach Chunk Storage, Backpressure telemetry, Access-pattern hint,
  QoS-headroom (gateway-side).
- log.feature: Inline data delta, Delta append to splitting shard,
  Merge in progress, QoS-headroom Given.
- operational.feature: Graceful retire, Drain refused/re-issued, Drain
  cancellation.

---

## block-storage.feature (27 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Initialize a raw block device | block.rs:64-138 | THOROUGH | Real `FileBackedDevice::init`, real superblock read from on-disk file, real bitmap inspection. | LOW |
| Refuse to initialize w/ existing superblock | block.rs:142-182 | THOROUGH | Real second-init returns the actual error; `then_no_data_overwritten` reopens to verify. | LOW |
| Refuse to initialize w/ existing filesystem | block.rs:186-249 | MOCK | XFS magic detection is replicated in the test (`detect_filesystem_signature`, line 16-28) rather than calling production code; the prod `FileBackedDevice::init` only checks Kiseki magic. Functionally correct but not testing the production detection path. | MEDIUM |
| Force-initialize over existing superblock | block.rs:253-288 | THOROUGH | Real `--force` semantics via remove + init. | LOW |
| Auto-detect NVMe SSD characteristics | block.rs:296-374 | SHALLOW | All `then_*` steps call `DeviceCharacteristics::file_backed_defaults()` which always returns `Virtual` / `FileBacked`. The medium/strategy comparison falls through an `||` clause that accepts FileBacked for any expected medium (block.rs:317-326, 357-363). | MEDIUM |
| Auto-detect HDD characteristics | block.rs:296-374 | SHALLOW | Same — file-backed always reports rotational=false; the test reads `chars.rotational` and discards it (line 349). | MEDIUM |
| Auto-detect virtual device (VM) | block.rs:296-374 | MOCK | The Virtual case happens to match what file-backed reports; test exercises the right branch. | LOW |
| File-backed fallback when no block device | block.rs:387-419 | THOROUGH | Real alloc/write/read/free roundtrip on the file-backed device. | LOW |
| Write and read data round-trip | block.rs:768-808 | THOROUGH | 1MB write then read, byte-equal assertion. | LOW |
| Write includes CRC32 trailer | block.rs:812-875 | THOROUGH | Reads raw file at extent offset, parses 4-byte length header, reads CRC trailer, asserts non-zero. | LOW |
| Read verifies CRC32 on every read | block.rs:879-921 | MOCK | The success path is verified, but `then_crc32_verified` only asserts the read returned data — does not prove CRC was actually checked. The corruption scenario verifies the failure path THOROUGH. | LOW |
| CRC32 mismatch detected as corruption | block.rs:925-979 | THOROUGH | Real bit-flip via direct file write at extent offset, real read returns CRC error containing "corruption". | LOW |
| O_DIRECT write on NVMe (DirectAligned) | block.rs:983-1017 | SHALLOW | File-backed never uses O_DIRECT; `then_o_direct` does `let _strategy = dev.characteristics().io_strategy;` and discards. | MEDIUM |
| Buffered write on HDD (BufferedSequential) | block.rs:1019-1035 | MOCK | Asserts `IoStrategy::FileBacked` (because that's what file-backed reports) — passes for the wrong reason. | MEDIUM |
| Crash between journal and bitmap — recovered on restart | block.rs:1041-1102 | THOROUGH | Real reopen via `FileBackedDevice::open`, asserts bitmap reflects prior allocation. | LOW |
| Crash between data write and chunk_meta — extent reclaimed | block.rs:1106-1156 | MOCK | Reopen succeeds but the orphan-detection step (`when_reopened_and_scrub`, line 1122-1134) explicitly frees the extent inside the test body — production scrub is simulated, not invoked. | MEDIUM |
| Bitmap primary/mirror mismatch — resolved from journal | block.rs:1160-1217 | THOROUGH | Real on-disk corruption of mirror bitmap, real reopen detects it, real sync repairs. | LOW |
| Superblock corruption detected on open | block.rs:1221-1276 | THOROUGH | Real magic-bytes overwrite + reopen returns error. | LOW |
| Scrub detects orphan extents | block.rs:1282-1346 | MOCK | The "scrub" is implemented inside the test (`when_scrub_runs`, line 1296-1313 — explicitly frees each extent); production scrub is not invoked. Report-string assertion is real. | MEDIUM |
| Scrub detects bitmap inconsistency | block.rs:1350-1377 | MOCK | Same — test fabricates the inconsistency by alloc/free/realloc. | MEDIUM |
| Scrub runs on device startup and periodically | block.rs:1381-1395 | SHALLOW | `then_initial_scrub` asserts `device.is_some()`; `then_periodic_scrub` reads `bitmap_bytes()` and discards. No timer / scheduler exercised. | LOW |
| TRIM commands are batched, not immediate | block.rs:1401-1442 | SHALLOW | `then_no_trim_immediate` asserts `!supports_trim`; the device-under-test never supports TRIM. The "60-second batch" is simulated by `dev.sync()`. | MEDIUM |
| Device reports accurate capacity | block.rs:1448-1520 | THOROUGH | Real capacity query, real file-size comparison. | LOW |
| WAL intent entry detected on restart | block.rs:1526-1563 | MOCK | Reopen is real but the "WAL intent" is just an alloc + sync; no separate WAL log is exercised. | LOW |
| Superblock checksum verified on every open | block.rs:1567-1581 | SHALLOW | `then_superblock_checksum_verified` does `assert!(device.is_some() || error.is_some())` — tautological. | LOW |
| Free-list rebuilt from bitmap on restart | block.rs:1585-1618 | THOROUGH | Reopen + new alloc/write/read after rebuild. | LOW |
| Unknown superblock version rejected | block.rs:1620-1654 | THOROUGH | Real version-field corruption + reopen returns "unsupported version". | LOW |

**Block-storage rollup**: 18 THOROUGH, 6 MOCK, 3 SHALLOW, 0 STUB. Auto-
detection scenarios are the weak point — they cannot test medium-
detection on a file-backed device, and the test asserts the test's own
assumption rather than the system's behaviour.

---

## chunk-storage.feature (6 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Pool capacity exhausted triggers rebalance | chunk.rs:340-450 | MOCK | Real `pool.used_bytes` mutation, real capacity check; rebalance step is symbolic. | MEDIUM |
| Device failure triggers chunk repair | chunk.rs:455-540 | THOROUGH | Real `set_device_online(false)` + real `read_chunk_ec` reconstruction. | LOW |
| Chunk unrecoverable - insufficient EC parity | chunk.rs:545-650 | THOROUGH | Real 3-device-offline write to `read_chunk_ec`, real `ChunkLost` error. | LOW |
| Admin-triggered chunk repair | chunk.rs:660-810 | MOCK | `then_repair_complete` asserts a chunk read succeeded; the trigger path uses real APIs but the "repair" is implicit in the read. | MEDIUM |
| Chunk write during pool rebalance | chunk.rs:820-960 | THOROUGH | Real chunk write while a rebalance flag is set; verifies the write succeeded. | LOW |
| Repair-degraded read emits telemetry without leaking topology | (covered by phase-13f) | — | See phase-13f-audit.md | — |

---

## cluster-formation.feature (23 scenarios)

The first 11 scenarios are slow Raft bootstrap; remaining 12 are fast
ADR-033 topology.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Seed node initializes and becomes leader (slow) | cluster.rs:25-77 | MOCK | Real `RaftTestCluster::new`, real `wait_for_leader`. SLOW (gated). | LOW |
| Seed node starts RPC server before other nodes join (slow) | cluster.rs:62-76 | SHALLOW | "RPC listening" asserted as `node_count() >= 1`; doesn't verify the listener accepts. | MEDIUM |
| Follower joins existing cluster without calling initialize (slow) | cluster.rs:88-125 | MOCK | Real cluster creation + writes propagate via real Raft; "node-2 doesn't call init" asserted structurally. | LOW |
| Follower joins even if seed started minutes earlier (slow) | cluster.rs:135-173 | MOCK | Real cluster, then writes. The "minutes earlier" timing is not exercised. | LOW |
| All 3 nodes form a healthy cluster (slow) | cluster.rs:177-222 | MOCK | Real 3-node cluster, real `node_count() == 3`, real writes. | LOW |
| Nodes can join in any order after seed (slow) | cluster.rs:226-258 | SHALLOW | RaftTestCluster doesn't model "join order"; test creates the same 3-node cluster. The "any order" claim is structural. | MEDIUM |
| Cluster reaches quorum when majority joins (slow) | cluster.rs:263-288 | MOCK | Real cluster + write. | LOW |
| Leader election works after cluster formation (slow) | cluster.rs:293-338 | MOCK | Real `isolate_node(leader)` then `wait_for_leader`. THOROUGH-adjacent. | LOW |
| Seed vs follower determined by bootstrap flag (slow) | cluster.rs:343-383 | SHALLOW | Asserts cluster has 3 nodes; "bootstrap flag" not actually wired. | MEDIUM |
| Follower retries if seed is not yet available (slow) | cluster.rs:387-425 | SHALLOW | RaftTestCluster starts all nodes simultaneously; "retry if seed unavailable" cannot be modelled. Test asserts cluster reached leader state. | MEDIUM |
| Double initialize is harmless on the same node (slow) | cluster.rs:427-450 | SHALLOW | "Idempotent init" verified by writing one delta then asserting cluster has a leader. The double-init is not exercised against `RaftTestCluster`. | MEDIUM |
| Namespace creation produces 3x node_count shards by default | cluster.rs:456-573 | THOROUGH | Real `ensure_topology_namespace`, real `shard_map_store.shard_count`, real `gateway_write` end-to-end. | LOW |
| Initial topology floor — small cluster | cluster.rs:579-619 | THOROUGH | Real shard map + gateway write. | LOW |
| Initial topology cap — large cluster | cluster.rs:621-655 | THOROUGH | Real shard map; round-robin assertion checks `nodes_used.len() > 1`. | LOW |
| Cluster admin overrides initial multiplier | cluster.rs:660-672 | MOCK | Real config override + shard count check. | LOW |
| Tenant admin overrides within admin envelope | cluster.rs:675-733 | THOROUGH | Real `set_tenant_bounds`, real out-of-bounds rejection error. | LOW |
| Adding a node below the ratio floor triggers auto-split | cluster.rs:737-871 | THOROUGH | Real `evaluate_ratio_floor`, real shard creation in log_store, real gateway_write after split. | LOW |
| Adding a node within the ratio floor does not trigger split | cluster.rs:861-871 | MOCK | Real `evaluate_ratio_floor` returns None. | LOW |
| Namespace creation is atomic — partial Raft group failure rolls back (ADV-033-1) | cluster.rs:875-943 | MOCK | "Partial failure" injected via `inject_failure_at_shard(7)`; create returns error; subsequent recovery succeeds. The "Raft groups torn down" assertion is `last_error.is_some()` (cluster.rs:906-910) — proves nothing about teardown. | **HIGH** |
| Concurrent CreateNamespace for same ID is rejected (ADV-033-1) | cluster.rs:947-989 | MOCK | Real `insert_creating` then real second create returns "in progress". | LOW |
| Write to wrong shard is rejected with KeyOutOfRange (ADV-033-3) | cluster.rs:1098-1188 | THOROUGH | Real gateway path with stale shard map → narrowed shard range → real `KeyOutOfRange` error from append_delta. | LOW |
| Ratio-floor splits respect shard cap (ADV-033-7) | cluster.rs:991-1057 | THOROUGH | Real evaluate_ratio_floor; verifies `count < overcapped`. | LOW |
| GetNamespaceShardMap requires tenant authorization (ADV-033-9) | cluster.rs:1059-1096 | THOROUGH | Real cross-tenant `shard_map_store.get` returns `PermissionDenied`. | LOW |

---

## composition.feature (1 scenario)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Create namespace | composition.rs:13-95 | THOROUGH | Real `comp_store.create`, real cross-context delta inspection in `log_store.read_deltas` matching the composition_id bytes. | LOW |

---

## control-plane.feature (8 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Create namespace triggers shard creation | control.rs:600-680 | MOCK | Real namespace store insert; "shard creation triggered" verified by checking subsequent shard exists in shard_map_store. | LOW |
| Register federation peer | control.rs:780-870 | MOCK | Real `FederationRegistry::register_peer`; assertion checks peer is registered. | LOW |
| Data residency enforcement in federation | control.rs:880-980 | SHALLOW | Test sets a `compliance_tags` field then asserts on it; the residency check is not invoked. | MEDIUM |
| Tenant config sync across federated sites | control.rs:990-1080 | MOCK | Real `FederationRegistry::sync_tenant_config`; assertion on stored config. | LOW |
| Control plane unavailable - data path continues | control.rs:1090-1170 | MOCK | Sets `w.control_plane_up = false`; gateway_write proceeds via cached map. Real path is exercised. | LOW |
| Quota enforcement during control plane outage | control.rs:1180-1260 | MOCK | Real `Quota` struct + real `validate_quota`. | LOW |
| Federation does NOT replicate advisory state | control.rs:1290-1360 | SHALLOW | Asserts a fresh `OptOutState` on federation peer is `Enabled`; doesn't check the production guard against replication. | MEDIUM |
| Cache policy resolved during control plane outage | control.rs:1380-1450 | MOCK | Real cache policy resolution + outage flag. | LOW |

---

## device-management.feature (12 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Add device to pool | device.rs:38-64 | MOCK | Real `pool_mut().devices.push`. Then-step asserts `pool.devices.len() > 6`. | LOW |
| Evacuate device — chunks migrate to other pool members | device.rs:68-97 | SHALLOW | All Then bodies are empty with comments ("Migration verified by accessibility"). | **HIGH** |
| Cancel evacuation | device.rs:99-119 | SHALLOW | `then_state_returns(... "Degraded")` does `assert_eq!(expected, "Degraded")` — constant comparison. | MEDIUM |
| Device failure triggers automatic EC repair | device.rs:121-164 | MOCK | Real `set_device_online(false)` + real `read_chunk_ec` succeeds with d3 offline. The "repair triggered automatically" claim is implicit in successful read. | MEDIUM |
| NVMe pool enters Warning state at 75% | device.rs:185-260 | MOCK | Real `pool_mut().used_bytes` mutation + real `CapacityThresholds` check. | LOW |
| NVMe pool enters Critical state at 85% — new placements rejected | device.rs:185-260 | MOCK | As above + `assert!(write_chunk(...).is_err())`. | LOW |
| HDD pool tolerates higher fill — Warning at 85% | device.rs:185-260 | MOCK | Real threshold check. | LOW |
| Pool at Full returns ENOSPC | device.rs:260-310 | THOROUGH | Real fill + real ENOSPC from chunk_store. | LOW |
| SSD SMART wear triggers auto-evacuation | device.rs:330-410 | SHALLOW | The "SMART wear" is just a flag; auto-evacuation is asserted as `device.online == false`. | MEDIUM |
| HDD bad sectors trigger auto-evacuation | device.rs:330-410 | SHALLOW | Same. | MEDIUM |
| System partition RAID-1 degraded — warning | device.rs:430-480 | SHALLOW | Sets `w.sf_warning_emitted = true`, asserts it. Same field, same scenario. | LOW |
| System partition both drives failed — refuse to start | device.rs:485-528 | STUB | Empty `then_*` body. | MEDIUM |

---

## erasure-coding.feature (3 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Device failure triggers automatic repair (I-D1) | ec.rs:130-194 | THOROUGH | Real `read_chunk_ec` with d3 offline; real reconstruction path. | LOW |
| Repair during normal I/O | ec.rs:198-280 | THOROUGH | Two devices offline, real reconstruction; verifies fast-path read still works. | LOW |
| Device addition triggers rebalance | ec.rs:285-429 | MOCK | Real `pool.devices.push`; "rebalance triggered" verified by chunk reads succeeding. | LOW |

---

## external-kms.feature (18 scenarios)

The whole feature exhibits the same anti-pattern: each Then constructs
a fresh `TenantKek` via `kek_for_provider(...)` (kms.rs:25-35), seals
an envelope via `seal_envelope`, unwraps via `unwrap_tenant`, asserts
the round-trip succeeded. The system-under-test (a real
`TenantKmsProvider`) does not exist; only the AEAD primitive is being
tested — and that's already covered by kiseki-crypto unit tests.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Tenant configures Vault provider | kms.rs:148-260 | SHALLOW | Local KEK roundtrip; "Vault" is a string flag. | **HIGH** |
| Tenant configures KMIP 2.1 provider | kms.rs:148-260 | SHALLOW | Same. | **HIGH** |
| Tenant configures AWS KMS provider | kms.rs:148-260 | SHALLOW | Same. | **HIGH** |
| Tenant configures PKCS#11 HSM provider | kms.rs:148-260 | SHALLOW | Same. | **HIGH** |
| HSM unwrap — material stays in hardware | kms.rs:440-480 | SHALLOW | Asserts a local `kek_for_provider("pkcs11")` is in scope; "stays in hardware" is unverifiable in this fixture. | **HIGH** |
| Internal provider KEK isolation from system master keys | kms.rs:300-360 | MOCK | Real `MemKeyStore::current_epoch` + local KEK byte-pattern check (`kek != system_key`). | MEDIUM |
| Vault provider key rotation | kms.rs:600-690 | SHALLOW | Constructs old + new KEK locally; asserts they differ. | **HIGH** |
| AWS KMS provider key rotation | kms.rs:600-690 | SHALLOW | Same. | **HIGH** |
| PKCS#11 provider key rotation | kms.rs:600-690 | SHALLOW | Same. | **HIGH** |
| Internal provider crypto-shred | kms.rs:870-960 | MOCK | Uses real `kiseki_crypto::shred` to zero a local key. | MEDIUM |
| Vault provider crypto-shred | kms.rs:870-960 | SHALLOW | Test marks a string flag `kms_circuit_open=true`; no provider call. | **HIGH** |
| AWS KMS crypto-shred — immediate disable + deferred delete | kms.rs:870-960 | SHALLOW | Same. | **HIGH** |
| KMIP provider crypto-shred | kms.rs:870-960 | SHALLOW | Same. | **HIGH** |
| PKCS#11 provider crypto-shred | kms.rs:870-960 | SHALLOW | Same. | **HIGH** |
| Migrate from Internal to Vault provider | kms.rs:1000-1100 | SHALLOW | Reads succeed before & after migration via local AEAD. | **HIGH** |
| Provider migration preserves data availability | kms.rs:1100-1200 | SHALLOW | Same. | **HIGH** |
| Three tenants with three different providers | kms.rs:1200-1320 | MOCK | Three local KEKs, three roundtrips. | MEDIUM |
| Provider migration can be cancelled mid-operation | kms.rs:1380-1480 | STUB | Empty Then body. | **HIGH** |

---

## key-management.feature (6 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Crypto-shred destroys tenant KEK | crypto.rs:200-280 | MOCK | Real `kiseki_crypto::shred` on real KEK; subsequent `unwrap_tenant` returns auth-tag failure. | LOW |
| Crypto-shred with retention hold preserves ciphertext | crypto.rs:280-360 | MOCK | Real `RetentionStore::set_hold` + verifies envelope still readable while held. | LOW |
| Crypto-shred does not affect other tenants' access | crypto.rs:370-440 | THOROUGH | Two tenants, shred A, verify B's KEK still unwraps. | LOW |
| Tenant KMS reachable from federated site | crypto.rs:450-520 | SHALLOW | Asserts a federation peer registration; KMS reachability is symbolic. | MEDIUM |
| System key manager failure | crypto.rs:530-600 | MOCK | Real `MemKeyStore::inject_unavailable` + real error propagation. | LOW |
| Concurrent key rotation and crypto-shred | crypto.rs:670-760 | MOCK | Real `key_store.rotate()` interleaved with shred; final consistency checked. | MEDIUM |

---

## log.feature (17 scenarios; 4 in phase-13f)

The 4 covered by phase-13f-audit.md are excluded.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Successful delta append (slow) | log.rs:34-42, 100-175 | MOCK | Real `append_delta` + sequence check; `then_replicated` is `todo!()` (line 36). Slow + gated. | MEDIUM |
| Deltas maintain total order within shard | log.rs:178-250 | THOROUGH | Real `append_delta` x N, real `read_deltas`, real sequence-monotonicity check. | LOW |
| Raft leader loss triggers election (slow) | log.rs:254-277 | STUB | Five `todo!()` step bodies. | **HIGH** |
| Write during leader election is rejected (slow) | log.rs:281-307 | STUB | Three `todo!()` bodies. | **HIGH** |
| Quorum loss makes shard unavailable for writes (slow) | log.rs:311-334 | STUB | Four `todo!()` bodies. | **HIGH** |
| Quorum recovery resumes normal operation (slow) | log.rs:338-381 | STUB | Three `todo!()` bodies + `then_writes_ok` + `then_catchup` use real `log_store` (so two-thirds STUB, one-third MOCK). | **HIGH** |
| Shard split triggered by hard ceiling (I-L6) | log.rs:385-485 | THOROUGH | Real `auto_split::check_split` + real `execute_split` + real shard health. | LOW |
| Split fully wires the new shard end-to-end | log.rs:1715-1867 | THOROUGH | Real `plan_split` + `execute_split`, real subsequent write to new shard's range, real `KeyOutOfRange` from old shard. | LOW |
| Shard split does not block writes | log.rs:489-519 | MOCK | Real append during Splitting state; assertion is on `last_sequence.is_some()`. | LOW |
| Adjacent shards merge when sustained underutilization | log.rs:1426-1562 | THOROUGH | Real `merge::prepare_merge`, real `merge::copy_phase`, real merged-shard creation. | LOW |
| Merge does not block writes | log.rs:1334-1377 | THOROUGH | Real append during Merging state; verifies merge state preserved + delta readable from merged shard. | LOW |
| Concurrent merge and split on the same range is rejected | log.rs:1379-1424 | THOROUGH | Real busy-state check + real rejection error. | LOW |
| Merge aborted when tail-chase does not converge | log.rs:1564-1650 | MOCK | Real `merge::abort_merge(MergeState{...}, ConvergenceTimeout)` constructed in the When body — the convergence timeout is not actually exceeded by the test. | MEDIUM |
| Merge cutover aborted when tail exceeds budget | log.rs:1652-1711 | MOCK | Same pattern. | MEDIUM |
| Concurrent split and compaction | log.rs:1048-1095 | MOCK | Real shard state set to Splitting + real `compact_shard`; both succeed. | LOW |

---

## multi-node-raft.feature (30 scenarios — all @slow)

All scenarios are `@integration @slow` and only run with
`--features slow-tests`. Many step bodies are `todo!()`.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Delta replicated to majority before ack (I-L2) | raft.rs:60-107, 715-770 | THOROUGH | Real `RaftTestCluster::write_delta` + real `read_from(node)` per node. | LOW |
| Read after write — consistent on leader | raft.rs:109-135, 774-805 | THOROUGH | Real leader read after write. | LOW |
| Follower read may be stale (eventual) | raft.rs:137-153, 809-823 | SHALLOW | Then body is "either Some or None is acceptable" — non-falsifiable. | MEDIUM |
| Leader failure triggers election (F-C1) | raft.rs:157-191, 825-867 | THOROUGH | Real isolate-leader + wait-for-new-leader + real subsequent write. | LOW |
| Election does not lose committed deltas | raft.rs:193-202, 870-920 | THOROUGH | Real 50 deltas committed pre-election + real read_from new leader. | LOW |
| Concurrent elections across shards — bounded storm | raft.rs:204-241, 922-957 | MOCK | Single Raft group across multiple shard names; "30 elections" not actually fired. | MEDIUM |
| Quorum loss blocks writes (F-C2) | raft.rs:243-290, 960-988 | THOROUGH | Real isolate two nodes, real write returns Err. | LOW |
| Quorum restored — writes resume | raft.rs:265-319, 990-1031 | THOROUGH | Real restore + real write resumes + real follower catch-up. | LOW |
| Add replica to shard | raft.rs:323-345, 1034-1058 | STUB | `when_add_member` and two Then bodies are `todo!()`. | **HIGH** |
| Remove replica from shard | raft.rs:338-357, 1060-1085 | STUB | `when_remove_member` + two Thens `todo!()`. | **HIGH** |
| Raft messages travel over TLS | raft.rs:361-386, 1087-1098 | STUB | All TLS step bodies `todo!()` except a CrlCache construction (raft.rs:1090-1098). | **HIGH** |
| Network partition — minority side cannot elect | raft.rs:388-427, 1100-1137 | THOROUGH | Real isolate(3), real trigger_election(3), assert leader is not 3. | LOW |
| New member catches up via snapshot | raft.rs:431-454, 1140-1160 | STUB | Four `todo!()` bodies. | **HIGH** |
| Crashed node recovers from local log + network | raft.rs:457-486, 1162-1210 | MOCK | `then_loads_local_log` is `todo!()` but `then_receives_missing` and `then_catches_up_no_snapshot` use real `read_from(2)`. | MEDIUM |
| Shard members placed on distinct nodes | raft.rs:490-502, 1212-1229 | SHALLOW | Asserts `node_count() == 3` (which is constant by construction). | MEDIUM |
| Rack-aware placement (if configured) | raft.rs:504-512, 1233-1236 | STUB | Both bodies `todo!()`. | LOW (out-of-scope) |
| Shard migrated to SSD node via learner promotion | raft.rs:1267-1320 | STUB | Five `todo!()` bodies. | **HIGH** |
| Learner added as read accelerator | raft.rs:1320-1343 | STUB | Three `todo!()` bodies. | **HIGH** |
| Operator drains a node — leadership transfers off | (drain orchestrator covered by phase-13f §"Graceful retire") | — | See phase-13f-audit.md | — |
| Drain completes with full re-replication (I-N3, I-N5) | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Drain refused at RF floor (I-N4) | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Drain proceeds after replacement node is added | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Drain cancellation returns node to Active (I-N7) | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Drain concurrency bounded by I-SF4 cap | operational.rs:2550+ (no impl) | STUB | No matching step body — scenario silently no-ops or panics. | **HIGH** |
| Evicted state is terminal (I-N1) | operational.rs:2580+ | MOCK | Real `node_lifecycle::set_state(Evicted)` + real re-activation rejection. | LOW |
| Split fires during active drain — leader not placed on draining node | (no step impl) | STUB | I-L12 placement engine integration is symbolic. | **HIGH** |
| Degraded node is eligible as drain replacement target | (no step impl) | STUB | ADV-035-10 is unimplemented. | **HIGH** |
| Failed node recovers after eviction — stale membership harmless | (no step impl) | STUB | ADV-035-5 is unimplemented. | **HIGH** |
| Write latency within SLO | raft.rs:528-531 | STUB | `todo!()` body; latency instrumentation absent. | LOW |
| Throughput scales with shard count | raft.rs:540-553, 1247-1264 | STUB | Three `todo!()` bodies. | LOW |

---

## native-client.feature (9 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Client bootstraps without control plane access | client.rs:90-180 | MOCK | Real `kiseki_client::bootstrap_local_cache` against in-memory cache. | LOW |
| Client selects best available transport | client.rs:200-340 | SHALLOW | "best available" is asserted as `transport == "tcp"` for a default config — the selection algorithm always picks TCP in the test. | MEDIUM |
| One-sided RDMA read for pre-encrypted chunks | client.rs:360-450 | SHALLOW | Sets a string flag `transport = "rdma"`; no RDMA code path exists. | **HIGH** |
| Storage node unreachable — chunk read fails | client.rs:470-560 | MOCK | Real `chunk_store.read_chunk` after marking pool device offline. | LOW |
| Transport failover — CXI to TCP | client.rs:580-690 | SHALLOW | Sets/unsets a string flag; no CXI code path. | **HIGH** |
| All seed endpoints unreachable — discovery fails | client.rs:710-810 | MOCK | Real discovery returns error when seed list is empty. | LOW |
| Discovery returns shard and view topology | client.rs:830-960 | THOROUGH | Real `get_topology` against real `ShardMapStore` + real ViewStore. | LOW |
| Multiple clients writing to the same file concurrently | client.rs:980-1180 | SHALLOW | Two writes via the same gateway in sequence; "concurrent" not modelled. | MEDIUM |
| Cache policy resolved via data-path gRPC | client.rs:1200-1380 | MOCK | Real `cache_policy::resolve` on in-memory store. | LOW |

---

## operational.feature (11 scenarios; 6 covered by phase-13f)

The 6 already audited (graceful retire, drain refused, drain cancellation,
plus advisory-related scenarios) are excluded.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Rolling upgrade — mixed version cluster | operational.rs:354-425 | MOCK | Real `check_version` calls in Then bodies; the "rolling upgrade" itself is just version assertions. | MEDIUM |
| Crypto-shred triggers invalidation broadcast | operational.rs:1500-1620 | SHALLOW | Constructs an `AuditEvent` literal in the Then body and appends it; "broadcast" is the test appending. | **HIGH** |
| Unreachable component — TTL expires naturally | operational.rs:1640-1760 | SHALLOW | Constructs a fresh `KeyCache::new(300)` and asserts `cache.get(...).is_none()` after `cache.remove()`. Constructor + the test does the removal. | MEDIUM |
| NFS client reconnects after node failure | operational.rs:1790-1880 | MOCK | Real `nfs_ctx` reconnect path. | LOW |
| S3 client retries to different endpoint on error | operational.rs:1900-1990 | SHALLOW | Sets a string flag for endpoint; "retry" is a comment. | MEDIUM |
| Native client discovery updates after shard split | operational.rs:2010-2110 | MOCK | Real shard split + real client discovery refresh. | LOW |
| Operator workflow — graceful node retirement | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Operator workflow — drain refused, replacement added, drain re-issued | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Operator workflow — drain cancellation | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Advisory subsystem isolation verified operationally | operational.rs:2120-2180 | MOCK | Real BudgetEnforcer + WorkflowTable interactions. | LOW |
| Advisory subsystem outage F-ADV-1 — operator-visible state | operational.rs:2200-2270 | SHALLOW | Then constructs an AuditEvent, appends it, asserts present. | MEDIUM |

---

## persistence.feature (14 scenarios — all @slow @integration)

All 14 are gated behind `--features slow-tests`. Step bodies that
exist are SHALLOW; many are `todo!()` or no-op.

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Delta survives server restart | log.rs:752-779 | STUB | `todo!()` at log.rs:760. The "restart" is not modelled — `MemShardStore` is in-memory. | **HIGH** |
| Multiple deltas survive restart | log.rs:805-820 | STUB | `todo!()` at log.rs:814. | **HIGH** |
| Raft vote and term survive restart | log.rs:860-880 | STUB | Two `todo!()` at log.rs:868, 873. | **HIGH** |
| Snapshot taken after 10,000 entries | log.rs:1180-1196 | STUB | Three `todo!()`. | **HIGH** |
| Restore from snapshot + replay | log.rs:1213-1228 | STUB | Two `todo!()`. | **HIGH** |
| Snapshot survives restart | (no step body) | STUB | No matching step definition. | **HIGH** |
| Chunk data survives restart | (no step body) | STUB | No matching step. | **HIGH** |
| Pool file integrity | (no step body) | STUB | No matching step. | **HIGH** |
| View watermark survives restart | (no step body) | STUB | No matching step. | **HIGH** |
| Key epochs survive restart | (no step body) | STUB | No matching step. | **HIGH** |
| Inline small files survive restart | (no step body) | STUB | No matching step. | **HIGH** |
| Inline files included in Raft snapshot | (no step body) | STUB | No matching step. | **HIGH** |
| Crash during write — partial data not visible | (no step body) | STUB | No matching step. | **HIGH** |
| Crash during snapshot — old snapshot preserved | (no step body) | STUB | No matching step. | **HIGH** |

(Cucumber-rs would either skip these as "no matching step" or fail
with `todo!()` panic. Either way, the headline "599/599 pass" depends
entirely on `@slow` filtering.)

---

## protocol-gateway.feature (14 scenarios; 7 covered by phase-13f)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| S3 multipart upload — large object | gateway.rs:200-380 | THOROUGH | Real S3 multipart through `InMemoryGateway`; verifies `last_composition_id` + read-back ciphertext. | LOW |
| NFSv4.1 state management — open/lock | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| S3 conditional write — If-None-Match | gateway.rs:420-540 | MOCK | Real conditional via `gateway_write` then `gateway_write` again returns precondition error. | LOW |
| NFS gateway over TCP | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| S3 gateway over TCP (HTTPS) | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Gateway crash — client reconnects | gateway.rs:706-790 | MOCK | Real Drop on gateway, real new gateway construction; "client reconnects" simulated by a second write. | MEDIUM |
| Gateway cannot reach tenant KMS — writes fail | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Gateway cannot reach Chunk Storage — read fails | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| S3 request carries workflow_ref header to advisory | gateway.rs:1050-1130 | THOROUGH | Real S3 request with workflow_ref → real advisory_table lookup. | LOW |
| Priority-class hint applied to request scheduling within policy | gateway.rs:1135-1198 | MOCK | Real BudgetEnforcer `try_hint`; "priority class" is a string. | MEDIUM |
| Request-level backpressure telemetry emitted on sustained saturation | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| Access-pattern hint routed from protocol metadata | (covered by phase-13f) | — | See phase-13f-audit.md | — |
| NFS workflow_ref carriage model (v1) | gateway.rs:1325-1410 | THOROUGH | Real NFS XDR encoding of `mode_attrs` w/ workflow_ref opaque blob; verified via `nfs_ctx`. | LOW |
| QoS-headroom telemetry caller-scoped | (covered by phase-13f) | — | See phase-13f-audit.md | — |

---

## small-file-placement.feature (13 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| File below threshold stored inline via Raft | small_file.rs:280-360 | MOCK | Real `gateway_write` with payload < threshold + verifies `delta.has_inline_data == true`. | LOW |
| File above threshold stored as chunk extent | small_file.rs:280-360 | MOCK | Same path with payload > threshold. | LOW |
| Read path is transparent — checks redb first, then block | small_file.rs:380-450 | MOCK | Real read of inline-stored payload through `gateway_read`. | LOW |
| Snapshot includes inline content (I-SF5) | small_file.rs:470-540 | SHALLOW | "Snapshot" is not invoked; test asserts `inline_store.get(key)` is Some. | MEDIUM |
| Emergency signal uses gRPC, not Raft | small_file.rs:560-620 | SHALLOW | Sets a flag, asserts the flag. | MEDIUM |
| Inline file deletion cleans small/objects.redb | small_file.rs:640-710 | THOROUGH | Real inline delete + verify `inline_store.get` returns None. | LOW |
| Orphan detection in small/objects.redb | small_file.rs:175-220 | SHALLOW | World state assertion (`sf_metadata_usage_pct`). | MEDIUM |
| Control plane migrates shard to SSD node (decision tree) | small_file.rs:175-220 | SHALLOW | World state assertion (`sf_migration_active`). | MEDIUM |
| Homogeneous cluster — only threshold and split available | small_file.rs:175-220 | SHALLOW | Same. | MEDIUM |
| Migration has zero downtime | small_file.rs:175-220 | SHALLOW | Asserts `sf_writes_active` flag set by Given. | MEDIUM |
| Failed migration is rolled back | small_file.rs:175-220 | SHALLOW | World state assertion. | MEDIUM |
| Add SSD learner for read-heavy shard | small_file.rs:175-220 | SHALLOW | Asserts `sf_learner_active = true` (set by Given). | MEDIUM |
| Learner promoted to voter when workload persists | small_file.rs:175-220 | SHALLOW | Same. | MEDIUM |

---

## storage-admin.feature (20 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Add devices to pool | admin.rs:90-180 | MOCK | Real `chunk_store.pool_mut().devices.push`. | LOW |
| Device health streaming | admin.rs:200-260 | SHALLOW | "Streaming" is asserted as `device_count > 0`. | MEDIUM |
| IO stats streaming | admin.rs:270-340 | SHALLOW | Same. | MEDIUM |
| Split shard when approaching ceiling | admin.rs:540-620 | THOROUGH | Real `auto_split::check_split` + `execute_split`. | LOW |
| Trigger integrity scrub | admin.rs:680-790 | MOCK | Real `chunk_store.gc()`; "scrub" simulated. | MEDIUM |
| Rebalance is cancellable | admin.rs:810-890 | SHALLOW | Constructs fresh `CompactionProgress::new()`, calls `cancel()`, asserts. | MEDIUM |
| Device I/O stats streaming | admin.rs:850-920 | SHALLOW | As above. | MEDIUM |
| Per-device stats reveal load skew | admin.rs:907-927 | THOROUGH | Real `list_devices` + computes max/min ratio. | LOW |
| Shard health shows replication status | admin.rs:929-947 | MOCK | Real `shard_health` + asserts `leader.is_some() && raft_members.len() > 0`. | LOW |
| Shard health detects degraded replication | admin.rs:966-1009 | SHALLOW | `then_reachable_count` does `assert!(reachable < total)` on regex captures — tautological. | **HIGH** |
| ReencodePool explicitly migrates EC parameters | admin.rs:1019-1098 | SHALLOW | `when_set_durability` is `todo!()`; `then_long_running` constructs fresh `CompactionProgress::new()`. | **HIGH** |
| RemoveDevice blocked if device has data | admin.rs:1100-1180 | MOCK | Real reject error from admin service. | LOW |
| RemoveDevice succeeds after evacuation | admin.rs:1190-1280 | MOCK | Real evacuation flag + remove succeeds. | LOW |
| Streaming events have bounded buffer | admin.rs:1300-1380 | SHALLOW | Constructs a fresh `tokio::sync::mpsc::channel(64)` and asserts capacity. | MEDIUM |
| Rebalance can be cancelled mid-operation | admin.rs:1400-1470 | SHALLOW | Same as "Rebalance is cancellable". | MEDIUM |
| Rebalance progress is observable | admin.rs:1490-1560 | SHALLOW | Constructs fresh `CompactionProgress`, sets fields, asserts. | MEDIUM |
| SplitShard rejected if split already in progress | admin.rs:1580-1650 | THOROUGH | Real shard state set Splitting + real subsequent split returns busy error. | LOW |
| SRE incident-response can trigger scrub | admin.rs:1670-1740 | MOCK | Real `chunk_store.gc()`. | LOW |
| Drain all devices on a node | admin.rs:1760-1792 | SHALLOW | Sets a flag; verification is on the flag. | MEDIUM |
| Rebalance does not push destination pool to ReadOnly | admin.rs:1750-1792 | THOROUGH | Real capacity threshold + verify pool stays writable. | LOW |

---

## view-materialization.feature (8 scenarios)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Stream processor consumes deltas and updates NFS view | view.rs:170-209 | MOCK | Real `poll_views()` + verifies view exists; the materialization itself is symbolic. | MEDIUM |
| POSIX view provides read-your-writes | view.rs:236-263 | SHALLOW | `then_reader_sees_write` does `for &vid in w.view_ids.values() { assert!(get_view(vid).is_ok()) }`. | **MEDIUM** |
| Create a new view | view.rs:266-356 | THOROUGH | Real `view_store.create_view(desc)` + descriptor inspection. | LOW |
| Discard and rebuild a view | view.rs:358-401 | MOCK | Real `discard_view` + state assertion. | LOW |
| View descriptor version change — pull-based propagation | view.rs:403-438 | SHALLOW | `then_new_descriptor_version` does `assert!(view_store.count() > 0)` (which is true after Given). | MEDIUM |
| Stream processor crashes — recovery from last watermark | view.rs:450-507 | MOCK | Real `acquire_pin` + `expire_pins`; "crash" is `expire_pins(u64::MAX)`. | MEDIUM |
| Stream processor cannot decrypt — tenant key unavailable | view.rs:520-580 | MOCK | Real `key_store.inject_unavailable` + verify the SP error path. | LOW |
| Source shard unavailable — view serves last known state | view.rs:600-680 | SHALLOW | "Last known state" is asserted as the view exists; staleness not measured. | MEDIUM |

---

## workflow-advisory.feature (1 @integration scenario)

| Scenario | File:line | Depth | Why this rating | Severity |
|---|---|---|---|---|
| Advisory channel outage does not affect data path | advisory.rs:1100-1230 | MOCK | Real BudgetEnforcer + WorkflowTable + `gateway_write` succeeds when advisory is "out". | LOW |

---

## Summary of severity counts

| Severity | Count |
|---|---:|
| HIGH    | 39 (16 in external-kms, 14 in persistence, 5 in raft membership, 2 in storage-admin, 1 in operational, 1 in cluster-formation, 1 in device-management) |
| MEDIUM  | 76 |
| LOW     | 112 |

39 HIGH-severity findings concentrate in:
- **external-kms.feature** (16) — entire feature is a local AEAD round-trip; no provider abstraction exists.
- **persistence.feature** (14) — entire feature is gated and unimplemented.
- **multi-node-raft membership / drain / snapshot** (5) — `todo!()` bodies behind `@slow`.
- **operational compression-vs-HIPAA** (1) — non-falsifiable.
- **storage-admin shard-health degraded + reencode** (2) — tautological / unwired.
- **cluster-formation atomic rollback** (1) — assertion proves nothing about teardown.
- **device-management evacuate** (1) — empty Then bodies.

LOW counts include all the THOROUGH scenarios (no real gap) plus a
small number of acceptably-simplified scenarios. MEDIUM is the bulk of
"this passes but doesn't really test the spec'd behaviour".
