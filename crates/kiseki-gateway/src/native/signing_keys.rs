//! HMAC signing keys derived from the system master key (ADR-042 §9.1).
//!
//! Three named keys are derived once at startup via HKDF-SHA256:
//!
//! - `handle_token_signing_key` — POSIX file handle tokens (§9, I-NG10).
//! - `dek_fetch_ticket_signing_key` — `TrustedCompute` DEK-fetch tickets (§8).
//! - `multipart_upload_id_signing_key` — multipart upload IDs (gate-1 N4).
//!
//! All three are scoped to the master-key epoch. On master-key rotation
//! the previous epoch's keys remain valid for `master_key_rotation_grace_ms`
//! (default 5 min) so in-flight tokens minted under the old epoch are
//! still accepted while clients migrate. Keys live in `Zeroizing<[u8; 32]>`
//! and are wiped on drop.
//!
//! Round-2 N5 from the gate-1 adversary noted that an earlier draft also
//! defined a `topology_signing_key`; that placeholder is omitted —
//! topology data is signed by the leader's mTLS identity and an extra
//! HMAC adds no defense against a tampered leader.

use aws_lc_rs::hkdf::{Salt, HKDF_SHA256};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;
use parking_lot::RwLock;
use std::collections::HashMap;
use zeroize::Zeroizing;

const SALT: &[u8] = b"kiseki-native-signing-keys-v1";
const INFO_HANDLE_TOKEN: &[u8] = b"kiseki-handle-token-v1";
const INFO_DEK_TICKET: &[u8] = b"kiseki-dek-fetch-ticket-v1";
const INFO_MULTIPART: &[u8] = b"kiseki-multipart-upload-id-v1";

struct OutputLen32;
impl aws_lc_rs::hkdf::KeyType for OutputLen32 {
    fn len(&self) -> usize {
        32
    }
}

/// Derived 32-byte HMAC keys for one master-key epoch.
pub struct EpochKeys {
    /// Master-key epoch these are derived from.
    pub epoch: KeyEpoch,
    /// HMAC-SHA256 key for POSIX `HandleToken`s.
    pub handle_token: Zeroizing<[u8; 32]>,
    /// HMAC-SHA256 key for `DekFetchTicket`s.
    pub dek_fetch_ticket: Zeroizing<[u8; 32]>,
    /// HMAC-SHA256 key for multipart upload IDs.
    pub multipart_upload_id: Zeroizing<[u8; 32]>,
}

impl EpochKeys {
    /// Derive the four signing keys from a master key via HKDF-SHA256.
    #[must_use]
    pub fn derive(master: &SystemMasterKey) -> Self {
        let salt = Salt::new(HKDF_SHA256, SALT);
        let prk = salt.extract(master.material());

        let mut handle = Zeroizing::new([0u8; 32]);
        prk.expand(&[INFO_HANDLE_TOKEN], OutputLen32)
            .and_then(|okm| okm.fill(&mut *handle))
            .expect("HKDF expand handle_token_signing_key");

        let mut dek = Zeroizing::new([0u8; 32]);
        prk.expand(&[INFO_DEK_TICKET], OutputLen32)
            .and_then(|okm| okm.fill(&mut *dek))
            .expect("HKDF expand dek_fetch_ticket_signing_key");

        let mut multipart = Zeroizing::new([0u8; 32]);
        prk.expand(&[INFO_MULTIPART], OutputLen32)
            .and_then(|okm| okm.fill(&mut *multipart))
            .expect("HKDF expand multipart_upload_id_signing_key");

        Self {
            epoch: master.epoch,
            handle_token: handle,
            dek_fetch_ticket: dek,
            multipart_upload_id: multipart,
        }
    }
}

/// Multi-epoch key store. Holds the current epoch's keys plus any
/// previous epochs still inside the rotation grace window.
pub struct SigningKeys {
    inner: RwLock<SigningKeysInner>,
    grace_ms: u64,
}

struct SigningKeysInner {
    current_epoch: KeyEpoch,
    keys: HashMap<KeyEpoch, EpochKeys>,
    /// Wall-clock millis when each retired epoch will leave the grace
    /// window. Entries on `keys` whose epoch is in this map are still
    /// valid; once the wall time passes the value, a future
    /// `prune_expired` call drops them.
    retired_at: HashMap<KeyEpoch, u64>,
}

impl SigningKeys {
    /// Build a new `SigningKeys` with one initial epoch from `master`.
    /// `grace_ms` is the rotation grace window — token validation accepts
    /// retired epochs whose wall-time still fits in the window.
    #[must_use]
    pub fn new(master: &SystemMasterKey, grace_ms: u64) -> Self {
        let mut keys = HashMap::with_capacity(2);
        let epoch = master.epoch;
        keys.insert(epoch, EpochKeys::derive(master));
        Self {
            inner: RwLock::new(SigningKeysInner {
                current_epoch: epoch,
                keys,
                retired_at: HashMap::new(),
            }),
            grace_ms,
        }
    }

    /// Current (master-key) epoch — the epoch the gateway mints new
    /// tokens with.
    #[must_use]
    pub fn current_epoch(&self) -> KeyEpoch {
        self.inner.read().current_epoch
    }

    /// Look up the handle-token signing key for an epoch. Returns
    /// `None` when the epoch is unknown or has fallen out of grace.
    /// The returned `Vec<u8>` is a defensive copy; callers should
    /// scope its lifetime tightly. (HMAC signing copies key material
    /// internally so a fresh allocation is the safe shape.)
    #[must_use]
    pub fn handle_token_key(&self, epoch: KeyEpoch) -> Option<Vec<u8>> {
        let g = self.inner.read();
        g.keys.get(&epoch).map(|k| k.handle_token.to_vec())
    }

    /// Look up the DEK-fetch-ticket signing key for an epoch.
    #[must_use]
    pub fn dek_fetch_ticket_key(&self, epoch: KeyEpoch) -> Option<Vec<u8>> {
        let g = self.inner.read();
        g.keys.get(&epoch).map(|k| k.dek_fetch_ticket.to_vec())
    }

    /// Look up the multipart upload-id signing key for an epoch.
    #[must_use]
    pub fn multipart_upload_id_key(&self, epoch: KeyEpoch) -> Option<Vec<u8>> {
        let g = self.inner.read();
        g.keys.get(&epoch).map(|k| k.multipart_upload_id.to_vec())
    }

    /// Promote a new master key. The previous epoch is moved into the
    /// grace window; callers presenting tokens minted under the old
    /// epoch continue to validate until `grace_ms` elapses.
    pub fn rotate(&self, new_master: &SystemMasterKey, now_ms: u64) {
        let mut g = self.inner.write();
        let retiring = g.current_epoch;
        let new_epoch = new_master.epoch;
        if retiring == new_epoch {
            return;
        }
        g.keys.insert(new_epoch, EpochKeys::derive(new_master));
        g.current_epoch = new_epoch;
        g.retired_at.insert(retiring, now_ms.saturating_add(self.grace_ms));
    }

    /// Drop epochs whose grace window expired before `now_ms`.
    pub fn prune_expired(&self, now_ms: u64) {
        let mut g = self.inner.write();
        let to_drop: Vec<KeyEpoch> = g
            .retired_at
            .iter()
            .filter_map(|(e, deadline)| if *deadline <= now_ms { Some(*e) } else { None })
            .collect();
        for e in to_drop {
            g.keys.remove(&e);
            g.retired_at.remove(&e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn deterministic_derivation_per_epoch() {
        let m1 = SystemMasterKey::new([0x42; 32], KeyEpoch(7));
        let m2 = SystemMasterKey::new([0x42; 32], KeyEpoch(7));
        let k1 = EpochKeys::derive(&m1);
        let k2 = EpochKeys::derive(&m2);
        assert_eq!(*k1.handle_token, *k2.handle_token);
        assert_eq!(*k1.dek_fetch_ticket, *k2.dek_fetch_ticket);
        assert_eq!(*k1.multipart_upload_id, *k2.multipart_upload_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_keys_are_distinct() {
        let m = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let k = EpochKeys::derive(&m);
        assert_ne!(*k.handle_token, *k.dek_fetch_ticket);
        assert_ne!(*k.handle_token, *k.multipart_upload_id);
        assert_ne!(*k.dek_fetch_ticket, *k.multipart_upload_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rotation_keeps_old_epoch_during_grace() {
        let old = SystemMasterKey::new([0x01; 32], KeyEpoch(1));
        let store = SigningKeys::new(&old, 60_000);
        assert!(store.handle_token_key(KeyEpoch(1)).is_some());

        let new = SystemMasterKey::new([0x02; 32], KeyEpoch(2));
        store.rotate(&new, 1_000);
        assert_eq!(store.current_epoch(), KeyEpoch(2));
        // Both epochs valid during grace.
        assert!(store.handle_token_key(KeyEpoch(1)).is_some());
        assert!(store.handle_token_key(KeyEpoch(2)).is_some());

        // After grace, old epoch is gone.
        store.prune_expired(2_000_000);
        assert!(store.handle_token_key(KeyEpoch(1)).is_none());
        assert!(store.handle_token_key(KeyEpoch(2)).is_some());
    }
}
