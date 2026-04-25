# Phase 13f: Final 11 Fast-Suite Failures + 41 Slow-Suite Gaps

## Fast suite: 11 remaining failures

Each needs real subsystem infrastructure — no more wiring of existing code.

| Scenario | Blocker | What to build |
|---|---|---|
| NFS gateway over TCP | TCP transport | TCP listener + NFS RPC framing in kiseki-gateway |
| S3 gateway over TCP (HTTPS) | TCP transport | HTTPS listener (TLS + axum already exist, needs wiring) |
| NFSv4.1 state management — open/lock | NFS protocol | Expose SessionManager through NfsContext |
| Gateway cannot reach tenant KMS | KMS fault injection | Add inject_failure to MemKeyStore |
| Gateway cannot reach Chunk Storage | EC repair | Complete ChunkAvailabilityOps EC decode path |
| Delta with inline data below threshold | InlineStore | Wire SmallObjectStore into World + MemShardStore apply |
| QoS-headroom telemetry caller-scoped | Telemetry subscription | AdvisoryOps.subscribe_telemetry() |
| Request-level backpressure telemetry | Telemetry subscription | AdvisoryOps.subscribe_backpressure() |
| Access-pattern hint — readahead | Readahead trigger | Prefetch/readahead verification hook |
| Delta append to splitting shard | Split buffering | Buffer writes during shard split cutover |
| Merge does not block writes (last step) | Merge orchestration | Wire merged shard name in test setup |

### Recommended order

1. **NFS locking** (1 scenario) — expose `SessionManager::add_lock` through NfsContext
2. **KMS fault injection** (1) — add `inject_unavailable()` to MemKeyStore
3. **InlineStore** (1) — wire `SmallObjectStore` into World
4. **Merge last step** (1) — fix shard name registration in merge test
5. **Split buffering** (1) — buffer deltas during Splitting state
6. **EC repair** (1) — complete `read_chunk_ec` with fault injection
7. **Telemetry subscriptions** (2) — implement `subscribe_backpressure/telemetry` on advisory
8. **TCP transport** (2) — in-process TCP listener for NFS/S3
9. **Readahead** (1) — prefetch verification hook

## Slow suite (@slow, --features slow-tests): 41 remaining gaps

19 of 60 @slow scenarios pass (Raft harness works). 41 need APIs:

| Category | Count | What to build |
|---|---|---|
| Membership changes (add/remove voter) | 8 | RaftTestCluster::add_learner + change_membership |
| Snapshot transfer | 5 | TestNetwork::full_snapshot implementation |
| Persistent storage simulation | 3 | RedbRaftLogStore in test cluster |
| TLS transport inspection | 3 | Transport-level message hooks |
| Rack-aware placement | 3 | Rack topology metadata on nodes |
| Drain orchestration | 8 | Control plane drain protocol |
| Learner support | 2 | RaftTestCluster::add_learner |
| Performance measurement | 2 | Latency/throughput instrumentation |
| SSD migration | 1 | Storage tier metadata |
| Node recovery | 2 | Persistent log + network recovery |
| Concurrent elections | 1 | Multi-shard cluster (30 shards) |
| Follower reads | 1 | Read path verification on followers |
| Partition minority | 1 | Asymmetric partition + election verification |
| Network partition resilience | 1 | Partition simulation already works |

### Recommended order for slow suite

1. **Membership changes** (8) — add `add_learner()` + `change_membership()` to RaftTestCluster
2. **Learner support** (2) — same API, different scenarios
3. **Snapshot transfer** (5) — implement `full_snapshot` in TestNetwork
4. **Drain orchestration** (8) — needs control plane + Raft membership changes
5. **Node recovery** (2) — persistent log store in test cluster
6. **Performance** (2) — latency/throughput instrumentation
7. **TLS/rack/migration** (7) — transport + topology metadata

## Test infrastructure

- Fast suite: `cargo test -p kiseki-acceptance` (~80s, 181 scenarios)
- Slow suite: `cargo test -p kiseki-acceptance --features slow-tests` (all 241)
- Single scenario: `cargo test -p kiseki-acceptance -- -n "scenario name"`
