//! Per-tenant in-flight stream slot guard (ADR-042 gate-1 round-2 N1).
//!
//! The native server bounds the number of concurrent streams a single
//! tenant can hold (default 256, env `KISEKI_NATIVE_STREAM_CAP`). The
//! client cooperates by acquiring a [`StreamSlot`] before opening a
//! streaming RPC and releasing it when the operation completes —
//! including on panic, future drop, or task cancellation. The
//! `StreamSlot` is an RAII guard: `Drop` always decrements, so a
//! panic mid-stream never leaks a slot.
//!
//! The shared counter is a `DashMap<TenantId, Arc<AtomicUsize>>` so
//! per-tenant contention does not serialize across tenants.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use kiseki_common::ids::OrgId;

/// Process-wide stream-slot counter, sharded by tenant.
#[derive(Debug, Default)]
pub struct StreamCapMap {
    counters: DashMap<OrgId, Arc<AtomicUsize>>,
    cap_per_tenant: usize,
}

impl StreamCapMap {
    /// Build a new map with the given per-tenant cap. `cap == 0` means
    /// "no cap".
    #[must_use]
    pub fn new(cap_per_tenant: usize) -> Self {
        Self {
            counters: DashMap::new(),
            cap_per_tenant,
        }
    }

    /// Try to acquire a stream slot for `tenant`. Returns `None` if
    /// the tenant has hit the cap; the caller should fail the RPC
    /// with `ResourceExhausted` (gRPC code 8).
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>, tenant: OrgId) -> Option<StreamSlot> {
        let counter = self
            .counters
            .entry(tenant)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        // Compare-and-swap loop so two threads can't both observe the
        // same `current < cap` and both increment past the cap.
        loop {
            let current = counter.load(Ordering::Acquire);
            if self.cap_per_tenant > 0 && current >= self.cap_per_tenant {
                return None;
            }
            let next = current + 1;
            if counter
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(StreamSlot {
                    counter: Some(counter),
                });
            }
            // Lost the race — retry.
        }
    }

    /// Snapshot the current in-flight count for `tenant`. Used by
    /// tests + metrics; the production cap-enforcement path uses
    /// `try_acquire` directly.
    #[must_use]
    pub fn current(&self, tenant: OrgId) -> usize {
        self.counters
            .get(&tenant)
            .map_or(0, |c| c.load(Ordering::Acquire))
    }
}

/// RAII guard. Holding one means the tenant's in-flight counter is
/// incremented by exactly 1; dropping it decrements. Cancellation,
/// panic, and future-drop all run `Drop` so the counter is always
/// balanced (gate-1 round-2 N1 — the explicit reason this guard
/// pattern was chosen over a manual decrement on the success path).
pub struct StreamSlot {
    /// `Option` so `Drop` can `take()` and skip a double-decrement
    /// on the (currently impossible) explicit-release path. `None`
    /// after `take`.
    counter: Option<Arc<AtomicUsize>>,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        if let Some(c) = self.counter.take() {
            c.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(tag: u8) -> OrgId {
        OrgId(uuid::Uuid::from_bytes([tag; 16]))
    }

    #[test]
    fn acquire_and_drop_balances_counter() {
        let m = Arc::new(StreamCapMap::new(4));
        assert_eq!(m.current(org(1)), 0);
        {
            let _slot = m.try_acquire(org(1)).expect("under cap");
            assert_eq!(m.current(org(1)), 1);
        }
        assert_eq!(m.current(org(1)), 0);
    }

    #[test]
    fn cap_enforced_per_tenant() {
        let m = Arc::new(StreamCapMap::new(2));
        let s1 = m.try_acquire(org(1)).expect("first");
        let s2 = m.try_acquire(org(1)).expect("second");
        assert!(m.try_acquire(org(1)).is_none(), "third must fail at cap");
        // Drop one and a fresh acquire succeeds.
        drop(s1);
        let s3 = m.try_acquire(org(1)).expect("after release");
        drop((s2, s3));
        assert_eq!(m.current(org(1)), 0);
    }

    #[test]
    fn caps_are_independent_across_tenants() {
        let m = Arc::new(StreamCapMap::new(1));
        let _a = m.try_acquire(org(1)).expect("alice");
        let _b = m.try_acquire(org(2)).expect("bob");
        // Tenant 1 hits cap; tenant 2 unaffected by tenant 1's slot.
        assert!(m.try_acquire(org(1)).is_none());
        assert_eq!(m.current(org(2)), 1);
    }

    #[test]
    fn panic_in_held_block_releases_slot() {
        let m = Arc::new(StreamCapMap::new(8));
        let m_clone = m.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _slot = m_clone.try_acquire(org(1)).unwrap();
            assert_eq!(m_clone.current(org(1)), 1);
            panic!("simulated work failure");
        }));
        assert!(result.is_err());
        // After the panic, the counter must have been decremented.
        assert_eq!(m.current(org(1)), 0);
    }

    #[test]
    fn cap_zero_means_unlimited() {
        let m = Arc::new(StreamCapMap::new(0));
        let mut held = Vec::new();
        for _ in 0..100 {
            held.push(m.try_acquire(org(1)).unwrap());
        }
        assert_eq!(m.current(org(1)), 100);
        held.clear();
        assert_eq!(m.current(org(1)), 0);
    }
}
