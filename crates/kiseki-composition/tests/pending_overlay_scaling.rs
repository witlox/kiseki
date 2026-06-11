//! #256 regression probe: per-id overlay removal must not scale with
//! overlay depth.
//!
//! The P4 overlay originally dropped name bindings via
//! `pending_names.retain(|_, v| v.0 != id)` — a full-map scan under
//! the write lock per drained id. A hydrator batch of B drains over an
//! overlay of N pending names did O(B×N) work while blocking every
//! ack-path bind (measured −71% PUT on GCP at the P4 boundary, GH
//! #256). The `by_id` index makes removal O(1).
//!
//! The assertion is a RATIO, not a wall-clock budget: draining 1k ids
//! out of a deep (50k) overlay must cost no more than 50× draining 1k
//! ids out of a shallow (1k) overlay. On the O(N) retain the ratio is
//! ~50× per the depth difference alone (measured ~100× in practice);
//! with the index it sits near 1×. The 50× line cleanly separates the
//! two regimes on any host, so the test is CI-safe despite timing.

use std::time::Instant;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId, ShardId};
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::NamespaceSizeBandPools;
use kiseki_composition::Namespace;

fn ns_id() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(0x256))
}

fn store_with_capacity(max: usize) -> CompositionStore {
    let store = CompositionStore::new().with_pending_max(max);
    store.add_namespace(Namespace {
        id: ns_id(),
        tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        shard_id: ShardId(uuid::Uuid::from_u128(2)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),
        size_band_pools: NamespaceSizeBandPools::default(),
    });
    store
}

/// Fill the overlay with `n` named volatile rows; return the first
/// `drain` ids created (the hydrator drains oldest-first).
fn fill(store: &CompositionStore, n: usize) -> Vec<CompositionId> {
    (0..n)
        .map(|i| {
            store
                .create_with_name_volatile(ns_id(), format!("key-{i:07}"), None, vec![], 64, None)
                .expect("volatile create")
        })
        .collect()
}

fn time_drains(store: &CompositionStore, ids: &[CompositionId]) -> f64 {
    let t = Instant::now();
    for id in ids {
        store.drain_pending(*id);
    }
    t.elapsed().as_secs_f64()
}

/// Min over `reps` fresh fill+drain measurements — a scheduler stall
/// inside any single window inflates that sample only; the min is the
/// uncontended cost (adversary review of #256, finding 2).
fn min_drain_secs(reps: usize, overlay: usize, drains: usize) -> f64 {
    (0..reps)
        .map(|_| {
            let store = store_with_capacity(overlay + drains);
            let ids = fill(&store, overlay);
            time_drains(&store, &ids[..drains])
        })
        .fold(f64::INFINITY, f64::min)
}

#[test]
fn drain_cost_does_not_scale_with_overlay_depth() {
    const DRAINS: usize = 1_000;
    const SHALLOW: usize = 1_000;
    const DEEP: usize = 50_000;
    const REPS: usize = 3;

    let shallow_s = min_drain_secs(REPS, SHALLOW, DRAINS.min(SHALLOW));
    let deep_s = min_drain_secs(REPS, DEEP, DRAINS);

    let ratio = deep_s / shallow_s.max(1e-9);
    eprintln!(
        "drain {DRAINS} ids (min of {REPS}): shallow({SHALLOW})={:.1}ms deep({DEEP})={:.1}ms ratio={ratio:.1}x",
        shallow_s * 1e3,
        deep_s * 1e3,
    );
    // 25x line: the O(overlay) retain measures 78-86x here (and
    // compresses toward the line on hosts where per-drain constant
    // overhead is heavy); the indexed regime sits at ~1x. 25x splits
    // the regimes with margin on both sides.
    assert!(
        ratio < 25.0,
        "per-id drain cost scales with overlay depth (ratio {ratio:.1}x ≥ 25x): \
         the O(overlay) scan under the pending_names write lock is back (GH #256)",
    );
}
