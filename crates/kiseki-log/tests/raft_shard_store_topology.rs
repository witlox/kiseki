#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `RaftShardStore` topology integration tests — split / merge / mutations.
//!
//! These tests exercise the `LogOps` trait surface on
//! `RaftShardStore` — the production multi-node wrapper that the
//! BDD `@library` scenarios never touch (they all run against
//! `MemShardStore`). The shape mirrors `MemShardStore`'s algorithm
//! tests but on the real Raft-replicated path:
//!
//! - `split_shard` / `merge_shards` per ADR-033 §3 / ADR-034
//! - `create_shard` (trait method) delegates to the inherent one
//! - `update_shard_range` / `set_shard_state` / `set_shard_config`
//!   persist via Raft consensus
//!
//! Tests use plain `#[test]` (not `#[tokio::test]`) because
//! `RaftShardStore` owns its own tokio runtime; nesting it inside an
//! outer tokio runtime panics on drop. Each test builds a Runtime
//! explicitly and `block_on`s the async assertions.
//!
//! Initially RED (the trait stubs returned `ShardNotFound` /
//! no-op'd). Now GREEN — `LogCommand::SetShardState`,
//! `UpdateShardRange`, `SetShardConfig` carry the mutations through
//! Raft, and the `LogOps` trait impl on `RaftShardStore` calls the
//! corresponding `OpenRaftLogStore::set_*` setters via the Raft
//! runtime.

use std::collections::BTreeMap;
use std::time::Duration;

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_control::shard_topology::{compute_initial_shards, ShardTopologyConfig};
use kiseki_log::error::LogError;
use kiseki_log::shard::{ShardConfig, ShardState};
use kiseki_log::traits::LogOps;
use kiseki_log::RaftShardStore;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(0x0073_a4d5_102e_u128))
}

fn make_shard_id(low: u128) -> ShardId {
    #[allow(clippy::unusual_byte_groupings)] // canonical UUID byte boundaries
    let base: u128 = 0x05ad_0000u128;
    ShardId(uuid::Uuid::from_u128(base + low))
}

/// Bind a TCP listener and return its port (releases the listener
/// before the Raft RPC server claims the port — there's a small
/// reuse race but acceptable for tests).
fn find_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Build a single-node `RaftShardStore` with one shard and wait for
/// the Raft group to elect itself leader. Caller drives async
/// assertions against the returned store via the `tokio::runtime::Runtime`
/// it provides.
fn single_node_store_with_shard(rt: &tokio::runtime::Runtime) -> (RaftShardStore, ShardId) {
    let port = find_port();
    let mut peers = BTreeMap::new();
    peers.insert(1u64, format!("127.0.0.1:{port}"));

    let store = RaftShardStore::new(1, peers, None);
    let shard_id = make_shard_id(1);
    store.create_shard(
        shard_id,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&format!("127.0.0.1:{port}")),
        true, // bootstrap = seed
    );
    // Wait for leader election.
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });
    (store, shard_id)
}

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// `LogOps::split_shard` on `RaftShardStore` must NOT return
/// `ShardNotFound` for a shard that we just created via
/// `RaftShardStore::create_shard`. Was RED while the trait default
/// fell through; now wired to a real impl that creates a new Raft
/// group, computes the midpoint, splits the range.
#[test]
fn split_shard_does_not_return_shard_not_found_for_existing_shard() {
    let rt = make_runtime();
    let (store, shard_id) = single_node_store_with_shard(&rt);
    let new_shard = make_shard_id(2);
    let result = LogOps::split_shard(&store, shard_id, new_shard, NodeId(1));
    assert!(
        !matches!(result, Err(LogError::ShardNotFound(_))),
        "RaftShardStore::split_shard returned ShardNotFound for a shard \
         that exists. The trait default is being used because \
         RaftShardStore doesn't override split_shard. \
         Implement the override per ADR-033 §3."
    );
}

/// `LogOps::merge_shards` on `RaftShardStore` must NOT return
/// `ShardNotFound` for two shards that we just created. Was RED
/// while the trait default fell through; now wired to a real impl
/// that extends the target's range to the union and marks the
/// source as `Retiring`.
#[test]
fn merge_shards_does_not_return_shard_not_found_for_existing_shards() {
    let rt = make_runtime();
    let port_a = find_port();
    let port_b = find_port();
    let mut peers = BTreeMap::new();
    peers.insert(1u64, format!("127.0.0.1:{port_a}"));

    let store = RaftShardStore::new(1, peers, None);
    let shard_a = make_shard_id(10);
    let shard_b = make_shard_id(11);
    store.create_shard(
        shard_a,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&format!("127.0.0.1:{port_a}")),
        true,
    );
    store.create_shard(
        shard_b,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
        Some(&format!("127.0.0.1:{port_b}")),
        false,
    );
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });

    let result = LogOps::merge_shards(&store, shard_a, shard_b);
    assert!(
        !matches!(result, Err(LogError::ShardNotFound(_))),
        "RaftShardStore::merge_shards returned ShardNotFound for two \
         shards that exist. The trait default is being used because \
         RaftShardStore doesn't override merge_shards. \
         Implement the override per ADR-034."
    );
}

/// `LogOps::create_shard` on `RaftShardStore` (the trait method) was
/// a no-op stub. After the fix, it delegates to the inherent
/// `create_shard` (which spawns a real Raft group) so subsequent
/// `shard_health` finds the shard.
#[test]
fn logops_create_shard_makes_shard_visible_to_shard_health() {
    let rt = make_runtime();
    let port = find_port();
    let mut peers = BTreeMap::new();
    peers.insert(1u64, format!("127.0.0.1:{port}"));

    let store = RaftShardStore::new(1, peers, None);
    let shard_id = make_shard_id(20);

    LogOps::create_shard(
        &store,
        shard_id,
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
    );
    rt.block_on(async { tokio::time::sleep(Duration::from_secs(2)).await });

    let result = rt.block_on(store.shard_health(shard_id));
    assert!(
        result.is_ok(),
        "RaftShardStore::create_shard (LogOps trait) failed to make \
         the shard visible: shard_health returned {result:?}. The \
         trait method must spawn a real Raft group via the inherent \
         create_shard.",
    );
}

/// `LogOps::update_shard_range` was `let _ = shard_id;` — a no-op.
/// Now wired through `LogCommand::UpdateShardRange` so the new range
/// is Raft-replicated and visible to subsequent `shard_health`.
#[test]
fn logops_update_shard_range_persists_change() {
    let rt = make_runtime();
    let (store, shard_id) = single_node_store_with_shard(&rt);

    let mut new_start = [0u8; 32];
    new_start[0] = 0x80; // upper half
    let new_end = [0xFFu8; 32];

    LogOps::update_shard_range(&store, shard_id, new_start, new_end);

    let info = rt.block_on(store.shard_health(shard_id)).unwrap();
    assert_eq!(
        info.range_start, new_start,
        "update_shard_range did not persist range_start through Raft.",
    );
    assert_eq!(
        info.range_end, new_end,
        "update_shard_range did not persist range_end through Raft.",
    );
}

/// ADR-033 §1 specifies a "sane default" for initial shard count:
/// `initial_shards = max(min(multiplier × node_count, shard_cap),
/// shard_floor)`. The formula is implemented in
/// `kiseki_control::shard_topology::compute_initial_shards` and
/// exercised by `@library` BDD scenarios — but no production caller
/// uses it. `crates/kiseki-server/src/runtime.rs:274` hardcodes
/// `bootstrap_shard = ShardId::from_u128(1)` and creates exactly one
/// shard regardless of cluster size.
///
/// For a 3-node cluster the ADR default produces 9 shards; the
/// runtime produces 1. This pins the formula's contract — the
/// follow-up bootstrap-wiring change can then assert against this.
#[test]
fn adr_033_default_for_3_node_cluster_yields_at_least_3_shards() {
    let cfg = ShardTopologyConfig::default();
    let n = compute_initial_shards(&cfg, 3);
    assert!(
        n >= 3,
        "ADR-033 §1 shard_floor default is 3; compute_initial_shards \
         returned {n} for a 3-node cluster — defaults must yield at \
         least the floor.",
    );
    assert_eq!(
        n, 9,
        "ADR-033 §1 with default multiplier=3 + 3 nodes should produce \
         9 shards (3 × 3 = 9, below cap of 64); got {n}. The runtime \
         currently ignores this formula and bootstraps a single shard, \
         giving up the 9× write-parallelism the ADR designs in.",
    );
}

/// `LogOps::set_shard_state` was a no-op stub. Now wired through
/// `LogCommand::SetShardState` so cutover transitions
/// (`Healthy` → `Splitting`/`Merging` → `Healthy`/`Retiring`) are
/// Raft-replicated.
#[test]
fn logops_set_shard_state_persists_change() {
    let rt = make_runtime();
    let (store, shard_id) = single_node_store_with_shard(&rt);

    LogOps::set_shard_state(&store, shard_id, ShardState::Splitting);

    let info = rt.block_on(store.shard_health(shard_id)).unwrap();
    assert_eq!(
        info.state,
        ShardState::Splitting,
        "set_shard_state did not transition the shard through Raft. \
         ADR-033 splits cannot complete without this state machine \
         working."
    );
}
