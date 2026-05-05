//! Client-side lease bookkeeping (ADR-042 §7).
//!
//! Tracks the leases this client currently holds; surfaces `LeaseFenced`
//! errors back to the caller; schedules background renewals at 1/3 of
//! the TTL. Phase 5 ships the in-memory store; the actual `RenewLease`
//! RPC dispatch is wired by the `NativeClient` once the gRPC channel
//! exists. The store is a pure data structure — testable in isolation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// One client-side lease record. Includes everything the renewal task
/// needs to dispatch a `RenewLease` RPC and surface an `Expired`
/// outcome to the caller.
#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub struct ClientLease {
    pub lease_id: [u8; 16],
    pub fencing_token: u64,
    /// Server-clamped TTL (the value to renew at).
    pub ttl: Duration,
    /// `Instant` at which the server says the lease expires; renewal
    /// fires at `expires_at - ttl/3 * 2` (i.e. at the 2/3 mark).
    pub expires_at: Instant,
    /// Whether the server has flagged this lease as fenced (a renewal
    /// or write came back with `LeaseFenced` / `LeaseExpired`). Once
    /// `true`, the manager refuses to vend the fencing token to the
    /// caller — they must re-acquire.
    pub fenced: bool,
}

impl ClientLease {
    /// Wall-clock instant at which the renewal task should fire next.
    /// 1/3 TTL margin so a renewal failure has 2/3 of the TTL to retry.
    #[must_use]
    pub fn next_renewal_at(&self) -> Instant {
        self.expires_at
            .checked_sub(self.ttl / 3)
            .unwrap_or(self.expires_at)
    }
}

/// In-memory lease registry. `Arc<LeaseManager>` is shared between the
/// `NativeClient` and the renewal background task.
#[derive(Default, Debug)]
pub struct LeaseManager {
    inner: Mutex<HashMap<[u8; 16], ClientLease>>,
}

impl LeaseManager {
    /// Build an empty manager wrapped in `Arc` for sharing with
    /// background renewal tasks.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Insert or refresh a lease record. Used after `AcquireLease` /
    /// successful `RenewLease`.
    pub fn record(&self, lease: ClientLease) {
        self.inner.lock().insert(lease.lease_id, lease);
    }

    /// Mark a lease as fenced so the next caller hits `LeaseFenced`
    /// instead of writing with a now-stale fencing token.
    pub fn mark_fenced(&self, lease_id: [u8; 16]) {
        if let Some(l) = self.inner.lock().get_mut(&lease_id) {
            l.fenced = true;
        }
    }

    /// Retire a lease record (release succeeded, or expiry was final).
    pub fn drop_lease(&self, lease_id: [u8; 16]) {
        self.inner.lock().remove(&lease_id);
    }

    /// Look up a lease's fencing token. Returns `None` when the lease
    /// is unknown OR fenced — both cases should fail the write.
    #[must_use]
    pub fn fencing_token(&self, lease_id: [u8; 16]) -> Option<u64> {
        let g = self.inner.lock();
        let l = g.get(&lease_id)?;
        if l.fenced {
            return None;
        }
        Some(l.fencing_token)
    }

    /// Snapshot every lease whose `next_renewal_at` is at or before
    /// `now`. The renewal task uses this to dispatch `RenewLease` RPCs
    /// in batch.
    #[must_use]
    pub fn due_renewals(&self, now: Instant) -> Vec<ClientLease> {
        self.inner
            .lock()
            .values()
            .filter(|l| !l.fenced && l.next_renewal_at() <= now)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: u8, ttl_ms: u64) -> ClientLease {
        ClientLease {
            lease_id: [id; 16],
            fencing_token: u64::from(id),
            ttl: Duration::from_millis(ttl_ms),
            expires_at: Instant::now() + Duration::from_millis(ttl_ms),
            fenced: false,
        }
    }

    #[test]
    fn record_then_fencing_token_returns_value() {
        let m = LeaseManager::new();
        m.record(lease(1, 1_000));
        assert_eq!(m.fencing_token([1; 16]), Some(1));
    }

    #[test]
    fn mark_fenced_hides_token() {
        let m = LeaseManager::new();
        m.record(lease(1, 1_000));
        m.mark_fenced([1; 16]);
        assert_eq!(m.fencing_token([1; 16]), None);
    }

    #[test]
    fn drop_lease_removes_record() {
        let m = LeaseManager::new();
        m.record(lease(1, 1_000));
        m.drop_lease([1; 16]);
        assert!(m.fencing_token([1; 16]).is_none());
    }

    #[test]
    fn due_renewals_picks_up_leases_past_one_third() {
        let m = LeaseManager::new();
        // ttl 1500 ms; expires_at = now+1500; renewal at expires_at - 500.
        let l = lease(1, 1_500);
        m.record(l.clone());
        // Now is before the renewal point, so no pickup.
        assert!(m.due_renewals(Instant::now()).is_empty());
        // Past the 2/3 mark: should pick up.
        assert_eq!(
            m.due_renewals(Instant::now() + Duration::from_millis(1_001))
                .len(),
            1
        );
    }

    #[test]
    fn due_renewals_skips_fenced_leases() {
        let m = LeaseManager::new();
        let l = lease(1, 100);
        m.record(l);
        m.mark_fenced([1; 16]);
        // Even past expiry, fenced leases are NOT renewed.
        assert!(m
            .due_renewals(Instant::now() + Duration::from_millis(200))
            .is_empty());
    }
}
