# Cluster Harness Follow-ups

**Created:** 2026-05-02

## Context

2026-05-01 landed:
- 3-node `ClusterHarness` (process-level singleton, kill+restart, owned-lock per scenario)
- `KISEKI_FABRIC_PEERS` env override (real bug: localhost multi-node was broken because fabric addr derivation reused the local data port for every peer)
- `GET /admin/chunk/{chunk_id_hex}` + `GET /admin/composition/{uuid}` admin endpoints
- `PersistentChunkStore::list_chunk_ids` (was a `Vec::new()` stub on the trait)
- 7 cross-node BDD scenarios promoted from `@library` to `@integration`
- 3 cross-node scenarios explicitly deferred with Gherkin comments stating what each needs
- 1 scenario moved from ClusterHarness back to `ServerHarness` baseline (1-node degenerate)

Validation: full `kiseki-acceptance` BDD suite + 21-test multi-node e2e
(`test_multi_node.py`, `test_cross_node_replication.py`,
`test_cluster_resilience.py`) all green against a real Docker
3-container cluster.

Released `v2026.40.534`. Two pre-existing `@library` flakes remain
(drain orchestrator + leader election randomness) — these pre-date
this session.

## Diamond required (1 item)

### F1 — Follower → leader S3 forwarding

Today `kiseki-log/src/raft/openraft_store.rs:344` collapses
openraft's `ForwardToLeader` into `LeaderUnavailable`. Smart clients
must look up the leader via `/cluster/info` and re-issue the request.
Real S3 clients (aws-cli, boto3) won't do that — they'll see 503,
back off, eventually fail.

The BDD `when_client_writes_1mb_to_node1` step works around this by
re-targeting the leader; production clients can't.

Diamond gates:
- **analyst** — does S3's HTTP 307 / `x-amz-bucket-region` cover
  leader routing? Is there a precedent for redirect-on-cluster-state?
- **architect** — gateway-level forwarding (server returns 307 with
  the leader's S3 URL) vs log-level forwarding (Raft follower's
  gateway proxies to the leader's gRPC + bridges back). Tradeoff:
  HTTP redirect leaks topology to the client; gRPC proxy hides it
  but doubles the hop budget.
- **adversary** — term-skew during failover, redirect loops, fan-out
  ordering, what happens when the "leader" we redirect to has just
  stepped down.
- **ADR** — likely a new ADR (042 next) covering the chosen path.

## Implementer-only (4 items)

### I1 — Python e2e dedup

ClusterHarness now covers the same ground as:

| File | Tests | Status |
|---|---|---|
| `tests/e2e/test_multi_node.py` | 6 | covered by BDD `@multi-node` |
| `tests/e2e/test_cross_node_replication.py` | 9 | covered by promoted `@cross-node` |
| `tests/e2e/test_cluster_resilience.py` | 6 | partially covered |

Slim each to assertions BDD genuinely can't make:
- Real disk persistence across container restart (BDD restarts processes, not disks)
- Cross-protocol roundtrips (S3 PUT → NFS GET) end-to-end through the kernel client
- Privileged-container behavior (kernel pNFS mount, ktls)

Keep untouched: `test_pnfs.py`, `test_fuse_client.py`,
`test_oidc_keycloak.py`, `test_vault_kms.py`, `test_perf_baseline.py`,
`test_tracing.py` — these all need real services or kernel primitives.

Outcome: shorter e2e runtime, single source of truth for cluster
behavior (BDD), e2e becomes "the kernel/cloud witness".

### I2 — NFSv3 @integration scenarios

NFSv3 client exists at `crates/kiseki-client/src/remote_nfs/v3.rs`;
the server listens on the NFS port; `nfs3-rfc1813.feature` has
scenarios but few or none drive the running server.

Add `@integration` scenarios in `nfs3-rfc1813.feature` against
single-node `ServerHarness` (NFSv3 is single-handle, no cluster
needed). Use the existing `nfs3_client()` helper on `ServerHarness`.

Mirror NFSv4 coverage shape: MOUNT, LOOKUP, READ, WRITE, COMMIT,
SETATTR with offset > 0 (the bug fixed 2026-05-01 in `40cac2b` —
catch a regression).

### I3 — GCP perf cluster re-run

Re-deploy via `infra/gcp/` (3 Terraform profiles —
default/transport/gpu) with the May 1 fixes:
- `40cac2b` NFS write buffering for offset > 0
- `19e1588` ETag on `CompleteMultipartUpload`
- `45ada7d` `KISEKI_FABRIC_PEERS` (irrelevant in containerized GCP,
  but proves no regression)

Outputs:
- A number (throughput / latency baseline)
- Confirmation the chunk replication quorum issue from the prior GCP
  run is fixed

This is validation, not code. Captures a baseline for the next
performance-focused cycle.

### I5 — NFSv3 CREATE+WRITE binding (bug discovered by I2)

While wiring the NFSv3 @integration scenarios (I2), the write-then-read
roundtrip surfaced a real bug:

`crates/kiseki-gateway/src/nfs3_server.rs:272` reads the file handle from
the WRITE RPC into `_fh` and discards it, then calls `ctx.write(data)`
which creates a *fresh* composition. The handle returned by the prior
CREATE never gets data attached, so a subsequent LOOKUP+READ on that
filename returns 0 bytes.

The unit tests in `nfs3_server::tests` cover WRITE-returns-OK in
isolation and don't catch this — only the end-to-end CREATE→WRITE→
LOOKUP→READ roundtrip surfaces it.

Fix options:
- Bind WRITE's data to the composition referenced by the supplied file
  handle (matches RFC 1813 §3.3.7 expectations).
- Have CREATE allocate a composition_id that WRITE then targets via
  the file-handle → composition_id table (`ctx.handles`).

Bugfix protocol: re-enable the 3 deferred NFSv3 scenarios in
`specs/features/nfs3-rfc1813.feature` (text preserved as comments) once
the binding lands.

### I4 — Pre-existing @library flake fixes

Two flakes were already present on `main` before this session and
showed up in the batched `cargo test -p kiseki-acceptance` run:

- `multi-node-raft.feature` "Leader failure triggers election (F-C1)"
  — step in `raft.rs` asserts the new leader is node-2 or node-3 but
  the in-process `RaftTestCluster`'s election occasionally picks the
  killed node back up after a fast restart-or-resurrect path.
  Diagnose: tighten the kill-or-wait sequence; the test is asserting
  on the correct invariant but allowing a small race window.
- `multi-node-raft.feature` "Drain concurrency bounded by I-SF4 cap"
  — `drain_raft.rs:633` "all draining nodes must have completed or
  been evicted" times out. The drain orchestrator's wait condition
  may be polling at the wrong cadence, or the test's deadline is too
  tight for the in-process cluster's election cadence.

Bugfix protocol: failing test first, find root cause, fix, audit
depth.

## Conditional (depends on intent)

### C1 — mTLS fixture for ClusterHarness

Currently `ClusterHarness` launches plaintext (no `KISEKI_TLS_*`
env vars set). Adding mTLS unblocks the deferred scenario "Tenant
cert presented to fabric port is rejected (I-Auth4)".

Decision needed before starting:
- **Diamond** if the fixture ships as a reusable "secure-mode" harness
  abstraction other tests will adopt (recommended — production
  deployments are mTLS-only).
- **Implementer** if it's a one-off cert bundle just for I-Auth4.

The existing `tests/e2e/gen-tls-certs.sh` is a starting point.

### C2 — Slow-node / fragment-fault injection

Two deferred scenarios need test-only knobs:
- "Composition delta arrives before fragment (D-10 cross-stream)"
  needs `KISEKI_TEST_FABRIC_SLOW_MS` or similar.
- "Read falls back to fabric when local fragment is missing" needs
  deterministic missing-fragment induction (e.g. an admin DELETE on
  a single fragment locally).

Defer entirely until either:
- A prod chaos primitive lands (operators run drills) — then it's a
  diamond on the prod feature, BDD scenarios fall out as adopters.
- A test-only knob is justified by enough scenarios depending on it
  — current count: 2. The session precedent we set was "we'd rather
  have prod knobs than test scaffolding" (see chunk-storage.feature
  DEFERRED comments).

## Suggested order

1. **I1** Python e2e dedup — fast win, reduces test drift  *(done 5225146)*
2. **I2** NFSv3 @integration scenarios — closes a known coverage gap
   *(NULL scenario landed b812e88; 3 roundtrip scenarios re-enabled by I5)*
3. **I4** Pre-existing flake fixes  *(done 297a19b — election-must-not-be-
   isolated-node + drain-orchestrator-bookkeeping)*
4. **I5** NFSv3 CREATE+WRITE binding  *(done — mirrored NFSv4
   buffer/flush in nfs3_server.rs + LOOKUP-after-WRITE in Nfs3Client;
   3 deferred roundtrip scenarios + cross-protocol scenario now green)*
5. **I3** GCP perf re-run — captures a baseline number
6. **F1** Follower → leader S3 forwarding — diamond, one ADR
7. **C1** mTLS fixture — likely diamond
8. **C2** revisit after #1–7 land

I3 + F1 can run in parallel (different domains).

## What NOT to do

- Do not promote any of the 3 `DEFERRED` cross-node scenarios with
  test-only knobs; the Gherkin comments document why.
- Do not change `ClusterHarness` to support more than 3 nodes without
  a real use case — the 3-node topology mirrors `docker-compose.3node.yml`
  and gives a clean 2-of-3 quorum.
- Do not remove `KISEKI_FABRIC_PEERS` even though hostnamed deployments
  don't need it; localhost multi-node (BDD, future single-host
  integration) does.
