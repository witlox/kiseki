#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit-level pin for the sync-adjacent FUSE paths surfaced by the
//! 2026-05-09 docker repro. The original symptom was "sync(1)
//! kills the kiseki-client daemon" but strace later showed the
//! daemon survives — the kernel-side `sync(2)` is what hangs,
//! cascading SIGTERM→SIGKILL when the test harness's outer timeout
//! fires.
//!
//! Even with that re-diagnosis, the sync-adjacent code paths
//! (FUSE_FLUSH, FUSE_FSYNC, FUSE_RELEASE) deserve explicit pin
//! tests — a panic in any of them would poison the daemon's
//! `RwLock` and turn the "kernel hangs" symptom into a real daemon
//! crash. This test drives the public `KisekiFuse` API through the
//! sequence the FUSE callbacks use during normal close + sync,
//! and asserts the lock-bearing state machine stays healthy.
//!
//! Path 3 of the 2026-05-09 sync investigation. Pinning lives
//! here so a future refactor that adds an `unwrap` inside the
//! state-machine ops fails the test before it's deployed.

use std::sync::Arc;

use kiseki_chunk::store::ChunkStore;
use kiseki_client::fuse_fs::KisekiFuse;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::InMemoryGateway;

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}
fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_fuse() -> KisekiFuse<InMemoryGateway> {
    let compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),

        size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
    KisekiFuse::new(gw, test_tenant(), test_namespace())
}

/// Pin: many open/write/release cycles in a row. A `RwLock`
/// poisoned during cycle N (e.g. by an `unwrap` panic in the
/// flush state machine) would surface as a panic on cycle N+1's
/// next lock acquisition. 32 cycles is well above any reasonable
/// kernel-issued sequence during a single `sync(1)`.
///
/// Each cycle does: create (mints inode + write_buffer), getattr
/// (lookup state), read (drives gateway through the cached path),
/// unlink (releases inode + drains buffer). That covers the same
/// state-machine surface as FUSE_OPEN→FUSE_WRITE→FUSE_FLUSH→FUSE_FSYNC→
/// FUSE_RELEASE→FUSE_UNLINK without needing the kernel.
#[test]
fn many_create_read_unlink_cycles_do_not_poison_state() {
    let mut fs = setup_fuse();
    for i in 0..32u8 {
        let name = format!("cycle-{i}.bin");
        let data = vec![i; 1024 + (i as usize) * 16];
        let ino = fs.create(&name, data.clone()).unwrap();
        let attr = fs.getattr(ino).expect("getattr at cycle");
        assert_eq!(attr.size as usize, data.len());
        let read = fs.read(ino, 0, data.len() as u32).expect("read at cycle");
        assert_eq!(read, data, "read mismatch at cycle {i}");
        fs.unlink(&name).expect("unlink at cycle");
    }
    // No state should leak after 32 cycles.
    let entries = fs.readdir();
    let kept: Vec<_> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.starts_with("cycle-"))
        .collect();
    assert!(
        kept.is_empty(),
        "all cycle files unlinked; readdir leaked: {kept:?}",
    );
}

/// Pin: a write that triggers the gateway path, followed by a
/// duplicate read of the same inode + range, leaves state intact.
/// The duplicate read mimics `sync` re-walking inodes that the
/// kernel already saw clean — the second read must not poison the
/// dirty-buffer state.
#[test]
fn duplicate_read_after_write_does_not_poison_state() {
    let mut fs = setup_fuse();
    let payload: Vec<u8> = (0..200u32).map(|i| (i & 0xff) as u8).collect();
    let ino = fs.create("dup-read.bin", payload.clone()).unwrap();
    for _ in 0..8 {
        let r = fs
            .read(ino, 0, payload.len() as u32)
            .expect("read after create+write");
        assert_eq!(r, payload);
    }
    // Lookup still works after repeated reads.
    let attr_via_lookup = fs.lookup("dup-read.bin").expect("lookup after reads");
    assert_eq!(attr_via_lookup.size as usize, payload.len());
    // The lookup-found attr matches the create-returned ino's attr.
    let attr_via_ino = fs.getattr(ino).expect("getattr same ino");
    assert_eq!(attr_via_ino.size, attr_via_lookup.size);
}

/// Pin: gateway-side `fsync_pending` returns `Ok` for the default
/// impl, so the daemon's `force_fsync` path (FUSE_FSYNC handler)
/// doesn't surface a spurious `EIO` to the kernel.
///
/// Pre-fix: if a future change to InMemoryGateway returns Err
/// from fsync_pending unexpectedly, the FUSE FSYNC handler maps
/// that to libc_eio() and userspace apps see fsync(2) failing
/// even when no I/O actually broke. This test pins the contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_fsync_pending_default_returns_ok() {
    use kiseki_gateway::ops::GatewayOps;
    let compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),

        size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools::default(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = Arc::new(InMemoryGateway::new(
        compositions,
        kiseki_chunk::arc_async(chunks),
        master_key,
    ));
    gw.fsync_pending().await.expect(
        "fsync_pending must succeed on a clean gateway; if it Errs, the \
         FUSE_FSYNC path will surface EIO to the kernel and userspace \
         apps will see a spurious fsync(2) failure",
    );
}
