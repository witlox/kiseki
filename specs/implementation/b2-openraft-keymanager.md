# B.2: openraft Integration for kiseki-keymanager

**Status**: Planned. **Created**: 2026-04-19.

## Overview

First real openraft integration. Pattern established here reused for
B.3 (log) and B.4 (audit). Separates log storage from state machine
per openraft's architecture.

## Implementation sequence

### Phase 1: Breaking trait change — async KeyManagerOps

Make `KeyManagerOps` async to support `raft.client_write().await` in
write methods. Breaking change for `MemKeyStore` and `KeyManagerGrpc`.

- `epoch.rs`: Add `#[tonic::async_trait]` to `KeyManagerOps`, make
  all methods `async fn`
- `store.rs`: Update `MemKeyStore` impl (wrap sync code in `async`)
- `grpc.rs`: Already async — no change needed
- `tests/epoch_scenarios.rs`: Add `#[tokio::test]` + `.await`

### Phase 2: Type definitions

- Add `Display` impl to `KeyCommand` (I-K8: redact key material)
- Create `raft/types.rs`: `KeyResponse` type, `declare_raft_types!`
- Create `raft/snapshot.rs`: serializable snapshot types

### Phase 3: Log storage

- Create `raft/log_store.rs`: `KeyLogStore` with `Arc<Mutex<Inner>>`
- Inner: `BTreeMap<u64, Entry>`, vote, last_purged_log_id
- Implement `RaftLogReader` + `RaftLogStorage`
- Call `IOFlushed` callback immediately (in-memory)

### Phase 4: State machine

- Create `raft/state_machine.rs`: `KeyStateMachine`
- Refactor existing `StateMachine::apply` logic
- Implement `RaftStateMachine` + `RaftSnapshotBuilder`
- Snapshot: serialize epochs to JSON, deserialize on restore

### Phase 5: Network stub + integration

- Create `raft/network.rs`: `StubNetworkFactory` (single-node)
- Refactor `RaftKeyStore` to wrap `Raft<KeyTypeConfig>` handle
- Reads from shared `Arc<Mutex<Inner>>`, writes via `client_write`
- Integration test: init → rotate → read → verify

## Key decisions

1. **Async trait**: `KeyManagerOps` becomes async. `MemKeyStore`
   trivially wraps sync code. This is the right long-term answer.

2. **Separate types**: Log store and state machine are separate types
   sharing no state. State machine provides reads via `Arc<Mutex>`.

3. **Generic log store**: After B.2 is proven, extract
   `MemLogStore<C>` into `kiseki-raft` for reuse in B.3/B.4.

4. **Snapshot format**: `serde_json` for simplicity. Key material
   is included (single-node is in-process). Production needs
   encryption (documented in adversarial gate).

## Dependencies

- `openraft = { workspace = true }` (already in workspace)
- `kiseki-raft = { path = "../kiseki-raft" }`
- `futures-util = "0.3"` (for `StreamExt` in `apply()`)
- `serde_json = "1"` (for snapshot serialization)
- `tonic` needs `async-trait` (already present via `#[tonic::async_trait]`)
