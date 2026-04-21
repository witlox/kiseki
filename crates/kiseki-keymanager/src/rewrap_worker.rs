//! Key rotation re-wrapping worker (I-K6).
//!
//! After a key rotation, old-epoch chunks must be re-encrypted under the
//! new epoch's DEK. This worker enumerates envelopes, decrypts with the
//! old master key, re-encrypts with the new master key, and tracks progress.
//! Supports cancellation and progress reporting.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::aead::Aead;
use kiseki_crypto::envelope::{self, Envelope};
use kiseki_crypto::keys::SystemMasterKey;
use zeroize::Zeroizing;

/// Progress state for a re-wrap operation.
#[derive(Debug)]
pub struct RewrapProgress {
    /// Total envelopes to re-wrap.
    pub total: AtomicU64,
    /// Envelopes successfully re-wrapped.
    pub completed: AtomicU64,
    /// Envelopes that failed re-wrap (retryable).
    pub failed: AtomicU64,
    /// Whether the worker has been cancelled.
    pub cancelled: AtomicBool,
}

impl RewrapProgress {
    /// Create a new progress tracker.
    #[must_use]
    pub fn new(total: u64) -> Self {
        Self {
            total: AtomicU64::new(total),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    /// Fraction complete (0.0 to 1.0).
    #[must_use]
    pub fn fraction_complete(&self) -> f64 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 1.0;
        }
        let completed = self.completed.load(Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)]
        {
            completed as f64 / total as f64
        }
    }

    /// Whether re-wrap is done (all completed or cancelled).
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
            || self.completed.load(Ordering::Relaxed) + self.failed.load(Ordering::Relaxed)
                >= self.total.load(Ordering::Relaxed)
    }

    /// Cancel the re-wrap operation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Re-encrypt an envelope from the old epoch's master key to the new epoch's.
///
/// Decrypts the ciphertext using the old master's DEK, then re-encrypts
/// under the new master's DEK. The chunk ID stays the same.
pub fn rewrap_envelope(
    aead: &Aead,
    old_master: &SystemMasterKey,
    new_master: &SystemMasterKey,
    env: &Envelope,
) -> Result<Envelope, RewrapError> {
    if old_master.epoch == new_master.epoch {
        return Err(RewrapError::SameEpoch);
    }
    if env.system_epoch != old_master.epoch {
        return Err(RewrapError::EpochMismatch {
            envelope: env.system_epoch,
            expected: old_master.epoch,
        });
    }

    // Decrypt with old key. Wrap in Zeroizing to clear plaintext from heap.
    let plaintext = Zeroizing::new(
        envelope::open_envelope(aead, old_master, env).map_err(|_| RewrapError::DecryptFailed)?,
    );

    // Re-encrypt with new key.
    let new_env = envelope::seal_envelope(aead, new_master, &env.chunk_id, &plaintext)
        .map_err(|_| RewrapError::EncryptFailed)?;

    Ok(new_env)
}

/// Run the re-wrap worker over a batch of envelopes.
///
/// Returns the re-wrapped envelopes. Updates `progress` atomically.
pub fn run_rewrap_batch(
    aead: &Aead,
    old_master: &SystemMasterKey,
    new_master: &SystemMasterKey,
    envelopes: &[Envelope],
    progress: &RewrapProgress,
) -> Vec<Envelope> {
    let mut results = Vec::new();

    for env in envelopes {
        if progress.cancelled.load(Ordering::Relaxed) {
            break;
        }

        match rewrap_envelope(aead, old_master, new_master, env) {
            Ok(new_env) => {
                results.push(new_env);
                progress.completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                progress.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    results
}

/// Re-wrap worker errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RewrapError {
    /// Cannot re-wrap to the same epoch.
    #[error("old and new epochs are the same")]
    SameEpoch,
    /// Envelope epoch doesn't match the old master key.
    #[error("envelope epoch {envelope:?} != expected {expected:?}")]
    EpochMismatch {
        /// Epoch on the envelope.
        envelope: KeyEpoch,
        /// Expected epoch (old master).
        expected: KeyEpoch,
    },
    /// Failed to decrypt with old master key.
    #[error("decrypt with old epoch key failed")]
    DecryptFailed,
    /// Failed to encrypt with new master key.
    #[error("encrypt with new epoch key failed")]
    EncryptFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::ChunkId;

    fn old_master() -> SystemMasterKey {
        SystemMasterKey::new([0x11; 32], KeyEpoch(1))
    }

    fn new_master() -> SystemMasterKey {
        SystemMasterKey::new([0x22; 32], KeyEpoch(2))
    }

    fn test_chunk_id() -> ChunkId {
        ChunkId([0xBB; 32])
    }

    #[test]
    fn rewrap_roundtrip() {
        let aead = Aead::new();
        let old = old_master();
        let new = new_master();
        let plaintext = b"hello kiseki rewrap";

        let env = envelope::seal_envelope(&aead, &old, &test_chunk_id(), plaintext).unwrap();
        assert_eq!(env.system_epoch, KeyEpoch(1));

        let new_env = rewrap_envelope(&aead, &old, &new, &env).unwrap();
        assert_eq!(new_env.system_epoch, KeyEpoch(2));

        // Verify decryption with new key yields original plaintext.
        let recovered = envelope::open_envelope(&aead, &new, &new_env).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn rewrap_same_epoch_rejected() {
        let aead = Aead::new();
        let old = old_master();
        let env = envelope::seal_envelope(&aead, &old, &test_chunk_id(), b"data").unwrap();

        let err = rewrap_envelope(&aead, &old, &old, &env);
        assert!(matches!(err, Err(RewrapError::SameEpoch)));
    }

    #[test]
    fn rewrap_epoch_mismatch_rejected() {
        let aead = Aead::new();
        let old = old_master();
        let new = new_master();
        let wrong = SystemMasterKey::new([0x33; 32], KeyEpoch(3));

        let env = envelope::seal_envelope(&aead, &wrong, &test_chunk_id(), b"data").unwrap();
        let err = rewrap_envelope(&aead, &old, &new, &env);
        assert!(matches!(err, Err(RewrapError::EpochMismatch { .. })));
    }

    #[test]
    fn batch_rewrap_with_progress() {
        let aead = Aead::new();
        let old = old_master();
        let new = new_master();

        let envelopes: Vec<Envelope> = (0..5)
            .map(|i| {
                let mut chunk_id = [0u8; 32];
                chunk_id[0] = i;
                envelope::seal_envelope(&aead, &old, &ChunkId(chunk_id), b"data").unwrap()
            })
            .collect();

        let progress = RewrapProgress::new(5);
        let results = run_rewrap_batch(&aead, &old, &new, &envelopes, &progress);

        assert_eq!(results.len(), 5);
        assert_eq!(progress.completed.load(Ordering::Relaxed), 5);
        assert_eq!(progress.failed.load(Ordering::Relaxed), 0);
        assert!(progress.is_done());

        // Verify each re-wrapped envelope decrypts correctly.
        for env in &results {
            assert_eq!(env.system_epoch, KeyEpoch(2));
            let plain = envelope::open_envelope(&aead, &new, env).unwrap();
            assert_eq!(plain, b"data");
        }
    }

    #[test]
    fn batch_rewrap_cancellation() {
        let aead = Aead::new();
        let old = old_master();
        let new = new_master();

        let envelopes: Vec<Envelope> = (0..10)
            .map(|i| {
                let mut chunk_id = [0u8; 32];
                chunk_id[0] = i;
                envelope::seal_envelope(&aead, &old, &ChunkId(chunk_id), b"data").unwrap()
            })
            .collect();

        let progress = RewrapProgress::new(10);
        progress.cancel();

        let results = run_rewrap_batch(&aead, &old, &new, &envelopes, &progress);
        assert!(results.is_empty());
        assert!(progress.is_done());
    }

    #[test]
    fn progress_fraction() {
        let p = RewrapProgress::new(100);
        assert!((p.fraction_complete() - 0.0).abs() < f64::EPSILON);

        p.completed.store(50, Ordering::Relaxed);
        assert!((p.fraction_complete() - 0.5).abs() < f64::EPSILON);

        p.completed.store(100, Ordering::Relaxed);
        assert!((p.fraction_complete() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn progress_empty_total() {
        let p = RewrapProgress::new(0);
        assert!((p.fraction_complete() - 1.0).abs() < f64::EPSILON);
        assert!(p.is_done());
    }
}
