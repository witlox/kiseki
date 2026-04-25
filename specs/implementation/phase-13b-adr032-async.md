# Phase 13b: ADR-032 Async GatewayOps + LogOps (completed 2026-04-25)

## Goal

Eliminate thread starvation under concurrent Raft consensus load by
making GatewayOps and LogOps traits async. Big-bang conversion: 24 files.

## Steps

| Step | Description | Status |
|------|-------------|--------|
| 1 | LogOps trait + MemShardStore/PersistentShardStore/RaftShardStore async | Done |
| 2 | Composition log bridge: emit_delta async (now returns Result) | Done |
| 3 | GatewayOps + InMemoryGateway async (tokio::sync::Mutex) | Done |
| 4 | S3 gateway: remove block_in_place, direct .await | Done |
| 5 | NFS gateway: block_in_place + block_on bridge | Done |
| 6 | FUSE client: same bridge pattern | Done |
| 7 | Tests: add .await to all gateway/log calls | Done |
| 8 | Cleanup | Partially done |

## Step 8 detail

| Item | Status |
|------|--------|
| Remove run_on_raft | Done |
| Remove block_in_place from s3_server | Done |
| KISEKI_RAFT_THREADS docs update | Not done |
| cargo clippy clean | Done (2026-04-25) |
| Concurrent S3 PUT verification (3-node cluster) | Not done |

## Key architectural decisions

- NFS/FUSE use `block_in_place` + `block_on` because NFS RPC handlers
  are sync (fuser/nfs-server crate constraints)
- emit_delta returns `Result<SequenceNumber, LogError>` instead of `bool`
  to propagate KeyOutOfRange and other errors
- Gateway propagates `GatewayError::KeyOutOfRange` from real append_delta
