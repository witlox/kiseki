//! In-process lease store (ADR-042 §7, I-NG10/I-NG12/I-NG14).
//!
//! Holds active write leases keyed by `(namespace_id, inode)`. Each
//! lease carries a fencing token (monotonic per-(tenant, namespace,
//! inode)) so a stale token from a renewed-but-expired holder is
//! rejected on subsequent writes (gate-1 F-H4 ordering).
//!
//! Phase 2 ships the in-process variant. Phase 4+ wires it behind a
//! Raft-replicated state machine (see ADR-042 §6 — dedup state + lease
//! state replicate together so a leader change preserves invariants).

use kiseki_common::ids::{NamespaceId, OrgId};
use parking_lot::Mutex;
use std::collections::HashMap;

/// What we hand back to the client on `AcquireLease` / `RenewLease`.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct LeaseGrant {
    pub lease_id: [u8; 16],
    pub fencing_token: u64,
    pub ttl_ms: u64,
    pub expires_at_millis_since_epoch: u64,
    pub holder_principal: String,
}

/// Internal record. Holders of `Mutex<LeaseStore>` lock briefly.
#[derive(Debug, Clone)]
struct LeaseRecord {
    lease_id: [u8; 16],
    fencing_token: u64,
    expires_at_millis_since_epoch: u64,
    holder_principal: String,
    /// Reserved for future use — replication of leases via Raft (Phase 4)
    /// will need to know the owning tenant to route the apply.
    #[allow(dead_code)]
    tenant_id: OrgId,
}

/// Outcome of an attempt to acquire.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum AcquireOutcome {
    Granted(LeaseGrant),
    /// Another holder has a non-expired lease.
    Held {
        holder_principal: String,
        ttl_remaining_ms: u64,
    },
    /// This node is in drain mode and refusing new leases.
    Draining { quiesce_window_remaining_ms: u64 },
}

/// Outcome of a renewal attempt.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum RenewOutcome {
    Renewed(LeaseGrant),
    /// Lease expired and someone else acquired.
    Expired {
        current_fencing_token: u64,
        current_holder_principal: String,
    },
    /// Lease ID does not match any known record.
    NotFound,
}

/// Outcome of a release attempt.
#[allow(missing_docs)]
#[derive(Debug)]
pub enum ReleaseOutcome {
    Released,
    NotFound,
    /// Another principal currently holds this lease — release rejected.
    NotHolder,
}

/// In-process lease store. See module docs.
#[derive(Debug, Default)]
pub struct LeaseStore {
    /// Active leases keyed by `(namespace_id, inode)`.
    leases: Mutex<HashMap<(NamespaceId, u64), LeaseRecord>>,
    /// Next fencing token for each `(tenant, namespace, inode)` tuple.
    /// Monotonically increases — token N+1 always > token N — so a
    /// renewed-then-expired holder's stale token compares less than
    /// the new holder's (Lamport pattern, I-NG12).
    fencing: Mutex<HashMap<(OrgId, NamespaceId, u64), u64>>,
    /// Drain mode: when set to `Some(quiesce_deadline_ms)` future
    /// `acquire()` calls return `Draining` until the deadline passes.
    drain_until_ms: Mutex<Option<u64>>,
    /// Quiesce window length when drain is initiated. Reported back to
    /// callers so they can decide between waiting or routing elsewhere.
    quiesce_window_ms: u64,
}

impl LeaseStore {
    /// Build a new lease store. `quiesce_window_ms` is what
    /// `begin_drain` reports back to clients during graceful shutdown.
    #[must_use]
    pub fn new(quiesce_window_ms: u64) -> Self {
        Self {
            leases: Mutex::new(HashMap::new()),
            fencing: Mutex::new(HashMap::new()),
            drain_until_ms: Mutex::new(None),
            quiesce_window_ms,
        }
    }

    /// Acquire a write lease. `now_ms` is the current wall clock, used
    /// for expiry; `ttl_ms` is what the caller asked for (server may
    /// clamp). `lease_id_seed` is the server-side opaque 16-byte ID.
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        inode: u64,
        principal: String,
        ttl_ms: u64,
        lease_id_seed: [u8; 16],
        now_ms: u64,
    ) -> AcquireOutcome {
        if let Some(deadline) = *self.drain_until_ms.lock() {
            if now_ms < deadline {
                return AcquireOutcome::Draining {
                    quiesce_window_remaining_ms: deadline - now_ms,
                };
            }
        }
        let mut leases = self.leases.lock();
        let key = (namespace_id, inode);
        if let Some(existing) = leases.get(&key) {
            if existing.expires_at_millis_since_epoch > now_ms {
                return AcquireOutcome::Held {
                    holder_principal: existing.holder_principal.clone(),
                    ttl_remaining_ms: existing.expires_at_millis_since_epoch - now_ms,
                };
            }
            // Expired — fall through to mint a new fencing token + lease.
        }
        let fencing_token = {
            let mut fenc = self.fencing.lock();
            let n = fenc.entry((tenant_id, namespace_id, inode)).or_insert(0);
            *n += 1;
            *n
        };
        let expires = now_ms.saturating_add(ttl_ms);
        let record = LeaseRecord {
            lease_id: lease_id_seed,
            fencing_token,
            expires_at_millis_since_epoch: expires,
            holder_principal: principal.clone(),
            tenant_id,
        };
        leases.insert(key, record);
        AcquireOutcome::Granted(LeaseGrant {
            lease_id: lease_id_seed,
            fencing_token,
            ttl_ms,
            expires_at_millis_since_epoch: expires,
            holder_principal: principal,
        })
    }

    /// Renew an existing lease. Same fencing token (renewal does NOT
    /// bump it — only a fresh acquire after expiry does). If the lease
    /// is unknown or someone else has acquired the same `(namespace,
    /// inode)` since, returns `Expired`.
    pub fn renew(
        &self,
        lease_id: [u8; 16],
        principal: &str,
        ttl_ms: u64,
        now_ms: u64,
    ) -> RenewOutcome {
        let mut leases = self.leases.lock();
        let mut found_key = None;
        for (key, rec) in leases.iter() {
            if rec.lease_id == lease_id && rec.holder_principal == principal {
                found_key = Some(*key);
                break;
            }
        }
        let Some(key) = found_key else {
            // Unknown lease ID OR lease ID exists but principal differs:
            // possibly the lease expired and a different principal has
            // acquired the (namespace, inode) since.
            return RenewOutcome::NotFound;
        };
        let rec = leases.get_mut(&key).expect("looked up above");
        if rec.expires_at_millis_since_epoch <= now_ms {
            // Expired — but the entry is still in the map. Tell the
            // caller about whatever's current; the entry stays as-is
            // until a fresh acquire overwrites it.
            return RenewOutcome::Expired {
                current_fencing_token: rec.fencing_token,
                current_holder_principal: rec.holder_principal.clone(),
            };
        }
        rec.expires_at_millis_since_epoch = now_ms.saturating_add(ttl_ms);
        RenewOutcome::Renewed(LeaseGrant {
            lease_id: rec.lease_id,
            fencing_token: rec.fencing_token,
            ttl_ms,
            expires_at_millis_since_epoch: rec.expires_at_millis_since_epoch,
            holder_principal: rec.holder_principal.clone(),
        })
    }

    /// Release a lease previously granted to `principal`. Returns
    /// `NotFound` if the lease ID is unknown, `NotHolder` if a
    /// different principal currently holds the matching lease ID.
    pub fn release(&self, lease_id: [u8; 16], principal: &str) -> ReleaseOutcome {
        let mut leases = self.leases.lock();
        let mut found_key = None;
        let mut wrong_principal = false;
        for (key, rec) in leases.iter() {
            if rec.lease_id == lease_id {
                if rec.holder_principal == principal {
                    found_key = Some(*key);
                } else {
                    wrong_principal = true;
                }
                break;
            }
        }
        match (found_key, wrong_principal) {
            (Some(k), _) => {
                leases.remove(&k);
                ReleaseOutcome::Released
            }
            (None, true) => ReleaseOutcome::NotHolder,
            (None, false) => ReleaseOutcome::NotFound,
        }
    }

    /// Validate a fencing token against the most recent lease for
    /// `(tenant, namespace, inode)`. Used by the write path BEFORE
    /// dedup short-circuit (gate-1 F-H4) so a stale-fencing-token write
    /// can't piggy-back on dedup.
    #[must_use]
    pub fn check_fencing_token(
        &self,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        inode: u64,
        presented_token: u64,
    ) -> bool {
        let fenc = self.fencing.lock();
        match fenc.get(&(tenant_id, namespace_id, inode)) {
            Some(current) => *current == presented_token,
            None => presented_token == 0, // No lease ever issued; only "0" is valid (no fencing).
        }
    }

    /// Initiate drain mode (I-NG14). Subsequent `acquire` calls return
    /// `Draining` until `now_ms + quiesce_window_ms`. Existing leases
    /// continue to operate.
    pub fn begin_drain(&self, now_ms: u64) {
        *self.drain_until_ms.lock() =
            Some(now_ms.saturating_add(self.quiesce_window_ms));
    }
}

#[cfg(test)]
#[allow(clippy::manual_let_else)]
mod tests {
    use super::*;

    fn org() -> OrgId {
        OrgId(uuid::Uuid::from_bytes([1; 16]))
    }
    fn ns() -> NamespaceId {
        NamespaceId(uuid::Uuid::from_bytes([2; 16]))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_grants_fresh_lease_with_fencing_token_1() {
        let s = LeaseStore::new(60_000);
        let outcome = s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_000);
        match outcome {
            AcquireOutcome::Granted(g) => {
                assert_eq!(g.fencing_token, 1);
                assert_eq!(g.expires_at_millis_since_epoch, 31_000);
                assert_eq!(g.holder_principal, "alice");
            }
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_acquire_while_held_returns_held() {
        let s = LeaseStore::new(60_000);
        let _ = s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_000);
        let outcome = s.acquire(org(), ns(), 42, "bob".into(), 30_000, [0xcd; 16], 1_500);
        assert!(matches!(outcome, AcquireOutcome::Held { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fencing_token_monotonically_increases_after_expiry() {
        let s = LeaseStore::new(60_000);
        let g1 = match s.acquire(org(), ns(), 42, "alice".into(), 1_000, [0xab; 16], 1_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        // Wall clock advances past lease expiry; bob acquires.
        let g2 = match s.acquire(org(), ns(), 42, "bob".into(), 1_000, [0xcd; 16], 5_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        assert!(g2.fencing_token > g1.fencing_token);
        // Alice's token is now stale.
        assert!(!s.check_fencing_token(org(), ns(), 42, g1.fencing_token));
        assert!(s.check_fencing_token(org(), ns(), 42, g2.fencing_token));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renew_keeps_same_fencing_token() {
        let s = LeaseStore::new(60_000);
        let g = match s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        match s.renew(g.lease_id, "alice", 30_000, 5_000) {
            RenewOutcome::Renewed(r) => {
                assert_eq!(r.fencing_token, g.fencing_token);
                assert_eq!(r.expires_at_millis_since_epoch, 35_000);
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renew_after_expiry_returns_expired() {
        let s = LeaseStore::new(60_000);
        let g = match s.acquire(org(), ns(), 42, "alice".into(), 1_000, [0xab; 16], 1_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        // Wait past expiry.
        let outcome = s.renew(g.lease_id, "alice", 30_000, 5_000);
        assert!(matches!(outcome, RenewOutcome::Expired { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_by_holder_succeeds() {
        let s = LeaseStore::new(60_000);
        let g = match s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        assert!(matches!(s.release(g.lease_id, "alice"), ReleaseOutcome::Released));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_by_other_principal_rejected() {
        let s = LeaseStore::new(60_000);
        let g = match s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_000) {
            AcquireOutcome::Granted(g) => g,
            _ => unreachable!(),
        };
        assert!(matches!(s.release(g.lease_id, "bob"), ReleaseOutcome::NotHolder));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_rejects_new_acquires() {
        let s = LeaseStore::new(60_000);
        s.begin_drain(1_000);
        assert!(matches!(
            s.acquire(org(), ns(), 42, "alice".into(), 30_000, [0xab; 16], 1_500),
            AcquireOutcome::Draining { .. }
        ));
    }
}
