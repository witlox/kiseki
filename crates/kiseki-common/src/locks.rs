//! Lock-poison helpers with structured tracing.
//!
//! Storage systems must respond differently to a poisoned lock
//! depending on what the lock protects.
//!
//! - **Data-path locks** (composition store, chunk index, allocator,
//!   raft state, file-handle map, dirty buffers): on poison, the
//!   structure may have a half-applied mutation. Continuing to use
//!   it silently risks reading from the wrong extent, double-
//!   allocating, or returning stale bytes — exactly what kiseki's
//!   durability promise forbids. Use [`LockOrDie::lock_or_die`]:
//!   emit a structured `tracing::error!` event then panic. The
//!   tokio runtime catches the panicked task and propagates it as
//!   a `JoinError` to the caller; the lock stays poisoned so any
//!   subsequent op on it also fails. The cluster's Raft + advisory
//!   layers route around the now-degraded node.
//!
//! - **Telemetry / metrics locks** (counters, histograms, audit
//!   buffers used only for observability): losing a few values is
//!   acceptable; recovering quietly is not, because it hides the
//!   upstream panic that poisoned the lock. Use
//!   [`LockOrWarn::lock_or_warn`]: emit a `tracing::warn!` event
//!   and recover via `PoisonError::into_inner`. Operators see the
//!   warning and can investigate the upstream panic.
//!
//! Both helpers carry a `name: &'static str` so the trace event
//! identifies WHICH lock poisoned. Combined with the file/line
//! that `tracing` records automatically, this is enough context
//! for an oncall to find the originating panic.
//!
//! Example:
//!
//! ```ignore
//! use kiseki_common::locks::{LockOrDie, LockOrWarn};
//!
//! // Data-path: panic on poison so the cluster routes around.
//! let chunks = self.chunks.lock().lock_or_die("persistent_store.chunks");
//!
//! // Telemetry: recover and keep serving.
//! let metrics = self.counters.lock().lock_or_warn("gateway.counters");
//! ```

use std::sync::LockResult;

/// Acquire a guard from a [`LockResult`] for a data-path lock.
///
/// On poison, emits a `tracing::error!` event with `name`, then
/// panics. The panic terminates the current tokio task (or thread);
/// the lock stays poisoned so subsequent ops also fail loudly.
pub trait LockOrDie<G> {
    /// Take the guard; panic if the lock is poisoned, after
    /// emitting a structured tracing event identifying the lock.
    fn lock_or_die(self, name: &'static str) -> G;
}

impl<G> LockOrDie<G> for LockResult<G> {
    #[inline]
    fn lock_or_die(self, name: &'static str) -> G {
        self.unwrap_or_else(|_e| {
            tracing::error!(
                lock = name,
                "data-path lock poisoned — a previous task panicked while holding it; \
                 aborting to protect data integrity (cluster will route around)",
            );
            panic!("data-path lock poisoned: {name}");
        })
    }
}

/// Acquire a guard from a [`LockResult`] for a telemetry / metrics
/// lock where a few stale or missed values are acceptable.
///
/// On poison, emits a `tracing::warn!` event with `name`, then
/// recovers the inner guard via `PoisonError::into_inner`.
/// Subsequent ops continue normally on the (possibly partially
/// updated) state.
pub trait LockOrWarn<G> {
    /// Take the guard; on poison, warn-trace and recover.
    fn lock_or_warn(self, name: &'static str) -> G;
}

impl<G> LockOrWarn<G> for LockResult<G> {
    #[inline]
    fn lock_or_warn(self, name: &'static str) -> G {
        self.unwrap_or_else(|e| {
            tracing::warn!(
                lock = name,
                "telemetry lock poisoned — a previous task panicked while holding it; \
                 recovering and continuing (some values may be lost or stale)",
            );
            e.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, RwLock};

    #[test]
    fn lock_or_die_returns_guard_on_healthy_mutex() {
        let m = Mutex::new(42);
        let g = m.lock().lock_or_die("test.mutex");
        assert_eq!(*g, 42);
    }

    #[test]
    fn lock_or_warn_returns_guard_on_healthy_mutex() {
        let m = Mutex::new(7);
        let g = m.lock().lock_or_warn("test.mutex");
        assert_eq!(*g, 7);
    }

    #[test]
    fn lock_or_die_works_on_rwlock_read() {
        let lock = RwLock::new(5);
        let g = lock.read().lock_or_die("test.rwlock.read");
        assert_eq!(*g, 5);
    }

    #[test]
    fn lock_or_die_works_on_rwlock_write() {
        let lock = RwLock::new(0);
        {
            let mut g = lock.write().lock_or_die("test.rwlock.write");
            *g = 99;
        }
        let g = lock.read().lock_or_die("test.rwlock.read");
        assert_eq!(*g, 99);
    }

    #[test]
    fn lock_or_warn_recovers_from_poisoned_mutex() {
        let m = std::sync::Arc::new(Mutex::new(10));
        let m_panic = std::sync::Arc::clone(&m);
        let h = std::thread::spawn(move || {
            let _g = m_panic.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        // Lock is now poisoned; warn-mode must still return a guard.
        let g = m.lock().lock_or_warn("test.recover");
        assert_eq!(*g, 10);
    }

    #[test]
    #[should_panic(expected = "data-path lock poisoned: test.die")]
    fn lock_or_die_panics_on_poisoned_mutex() {
        let m = std::sync::Arc::new(Mutex::new(0));
        let m_panic = std::sync::Arc::clone(&m);
        let h = std::thread::spawn(move || {
            let _g = m_panic.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        let _g = m.lock().lock_or_die("test.die");
    }
}
