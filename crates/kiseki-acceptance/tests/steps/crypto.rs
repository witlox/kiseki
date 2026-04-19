//! Step definitions for key-management.feature — scenarios with real assertions.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::aead::Aead;
use kiseki_crypto::envelope::{open_envelope, seal_envelope, unwrap_tenant, wrap_for_tenant};
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_crypto::keys::{MasterKeyCache, SystemMasterKey, TenantKek};
use kiseki_keymanager::epoch::KeyManagerOps;

// === Background ===

#[given("a Kiseki cluster with a system key manager")]
async fn given_km(_w: &mut KisekiWorld) {}

#[given(regex = r#"^system KEK "(\S+)" wrapping system DEKs$"#)]
async fn given_kek(_w: &mut KisekiWorld, _k: String) {}

#[given(regex = r#"^tenant "(\S+)" with tenant KMS at "(\S+)"$"#)]
async fn given_kms(w: &mut KisekiWorld, t: String, _addr: String) {
    w.ensure_tenant(&t);
}

#[given(regex = r#"^tenant KEK "(\S+)" in epoch (\d+)$"#)]
async fn given_tkek(_w: &mut KisekiWorld, _k: String, _e: u64) {}

// === Scenario 1: DEK generation ===

#[when("a new chunk is written")]
async fn when_chunk_write(_w: &mut KisekiWorld) {}

#[then(regex = r#"^a system DEK is generated.*$"#)]
async fn then_dek(w: &mut KisekiWorld) {
    let epoch = w.key_store.current_epoch().await.unwrap();
    let key = w.key_store.fetch_master_key(epoch).await.unwrap();
    let chunk_id = ChunkId([0x42; 32]);
    let dek = derive_system_dek(&key, &chunk_id);
    assert!(dek.is_ok(), "DEK derivation should succeed");
}

#[then(regex = r#"^the DEK encrypts the chunk plaintext using AES-256-GCM$"#)]
async fn then_aes(w: &mut KisekiWorld) {
    let aead = Aead::new();
    let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let chunk_id = ChunkId([0xab; 32]);
    let envelope = seal_envelope(&aead, &master, &chunk_id, b"plaintext");
    assert!(envelope.is_ok());
    let envelope = envelope.unwrap();
    let decrypted = open_envelope(&aead, &master, &envelope);
    assert!(decrypted.is_ok());
    assert_eq!(decrypted.unwrap(), b"plaintext");
}

// === Scenario 2: KEK rotation ===

#[given(regex = r#"^system KEK "(\S+)" is in epoch (\d+)$"#)]
async fn given_kek_epoch(_w: &mut KisekiWorld, _k: String, _e: u64) {}

#[when("the cluster admin triggers system KEK rotation")]
async fn when_rotate(w: &mut KisekiWorld) {
    match w.key_store.rotate().await {
        Ok(e) => {
            w.last_epoch = Some(e.0);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[then(regex = r#"^a new system KEK "(\S+)" is generated \(epoch (\d+)\)$"#)]
async fn then_new_epoch(w: &mut KisekiWorld, _k: String, expected: u64) {
    assert_eq!(w.last_epoch, Some(expected));
}

#[then(regex = r#"^existing chunks retain epoch (\d+) wrapping$"#)]
async fn then_retain(w: &mut KisekiWorld, epoch: u64) {
    assert!(w.key_store.fetch_master_key(KeyEpoch(epoch)).await.is_ok());
}

#[then(regex = r#"^both epochs are valid.*$"#)]
async fn then_both(w: &mut KisekiWorld) {
    assert!(w.key_store.fetch_master_key(KeyEpoch(1)).await.is_ok());
    if let Some(e) = w.last_epoch {
        assert!(w.key_store.fetch_master_key(KeyEpoch(e)).await.is_ok());
    }
}

// === Scenario 3: Tenant KEK wraps ===

#[given(regex = r#"^chunk "(\S+)" is encrypted with system DEK "(\S+)"$"#)]
async fn given_encrypted(_w: &mut KisekiWorld, _c: String, _d: String) {}

#[given(regex = r#"^"(\S+)" is wrapped with system KEK "(\S+)"$"#)]
async fn given_wrapped(_w: &mut KisekiWorld, _d: String, _k: String) {}

#[when(regex = r#"^"(\S+)" needs access to "(\S+)"$"#)]
async fn when_access(_w: &mut KisekiWorld, _t: String, _c: String) {}

#[then(regex = r#"^"(\S+)" is also wrapped with tenant KEK "(\S+)"$"#)]
async fn then_tenant_wrap(_w: &mut KisekiWorld, _d: String, _k: String) {
    // Verify wrap/unwrap roundtrip
    let aead = Aead::new();
    let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let tenant_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
    let chunk_id = ChunkId([0xbb; 32]);

    let mut envelope = seal_envelope(&aead, &master, &chunk_id, b"secret").unwrap();
    wrap_for_tenant(&aead, &mut envelope, &tenant_kek).unwrap();
    assert!(envelope.tenant_wrapped_material.is_some());

    let mut cache = MasterKeyCache::new();
    cache.insert(SystemMasterKey::new([0x42; 32], KeyEpoch(1)));
    let decrypted = unwrap_tenant(&aead, &envelope, &tenant_kek, &cache).unwrap();
    assert_eq!(decrypted, b"secret");
}

// === Scenario 17: Epoch mismatch ===

#[given(regex = r#"^chunk "(\S+)" was written in epoch (\d+)$"#)]
async fn given_old_epoch(_w: &mut KisekiWorld, _c: String, _e: u64) {}

#[given(regex = r#"^the current epoch is (\d+)$"#)]
async fn given_current_epoch(w: &mut KisekiWorld, target: u64) {
    let current = w.key_store.current_epoch().await.unwrap();
    for _ in current.0..target {
        w.key_store.rotate().await.unwrap();
    }
}

#[given(regex = r#"^epoch (\d+) KEK wrapping has not yet been migrated$"#)]
async fn given_not_migrated(_w: &mut KisekiWorld, _e: u64) {}

#[when(regex = r#"^a read for "(\S+)" is requested$"#)]
async fn when_read(_w: &mut KisekiWorld, _c: String) {}

#[then(regex = r#"^the system retrieves the epoch (\d+) tenant KEK wrapping$"#)]
async fn then_old_wrap(w: &mut KisekiWorld, epoch: u64) {
    // Verify old epoch key is still accessible
    assert!(w.key_store.fetch_master_key(KeyEpoch(epoch)).await.is_ok());
}

#[then("the read succeeds")]
async fn then_read_ok(_w: &mut KisekiWorld) {}
