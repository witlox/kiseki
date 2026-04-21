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
async fn given_km(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[given(regex = r#"^system KEK "(\S+)" wrapping system DEKs$"#)]
async fn given_kek(_w: &mut KisekiWorld, _k: String) {
    panic!("not yet implemented");
}

#[given(regex = r#"^tenant "(\S+)" with tenant KMS at "(\S+)"$"#)]
async fn given_kms(w: &mut KisekiWorld, t: String, _addr: String) {
    w.ensure_tenant(&t);
}

#[given(regex = r#"^tenant KEK "(\S+)" in epoch (\d+)$"#)]
async fn given_tkek(_w: &mut KisekiWorld, _k: String, _e: u64) {
    panic!("not yet implemented");
}

// === Scenario 1: DEK generation ===

#[when("a new chunk is written")]
async fn when_chunk_write(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

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
async fn given_kek_epoch(_w: &mut KisekiWorld, _k: String, _e: u64) {
    panic!("not yet implemented");
}

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
async fn given_encrypted(_w: &mut KisekiWorld, _c: String, _d: String) {
    panic!("not yet implemented");
}

#[given(regex = r#"^"(\S+)" is wrapped with system KEK "(\S+)"$"#)]
async fn given_wrapped(_w: &mut KisekiWorld, _d: String, _k: String) {
    panic!("not yet implemented");
}

#[when(regex = r#"^"(\S+)" needs access to "(\S+)"$"#)]
async fn when_access(_w: &mut KisekiWorld, _t: String, _c: String) {
    panic!("not yet implemented");
}

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
async fn given_old_epoch(_w: &mut KisekiWorld, _c: String, _e: u64) {
    panic!("not yet implemented");
}

#[given(regex = r#"^the current epoch is (\d+)$"#)]
async fn given_current_epoch(w: &mut KisekiWorld, target: u64) {
    let current = w.key_store.current_epoch().await.unwrap();
    for _ in current.0..target {
        w.key_store.rotate().await.unwrap();
    }
}

#[given(regex = r#"^epoch (\d+) KEK wrapping has not yet been migrated$"#)]
async fn given_not_migrated(_w: &mut KisekiWorld, _e: u64) {
    panic!("not yet implemented");
}

#[when(regex = r#"^a read for "(\S+)" is requested$"#)]
async fn when_read(_w: &mut KisekiWorld, _c: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^the system retrieves the epoch (\d+) tenant KEK wrapping$"#)]
async fn then_old_wrap(w: &mut KisekiWorld, epoch: u64) {
    // Verify old epoch key is still accessible
    assert!(w.key_store.fetch_master_key(KeyEpoch(epoch)).await.is_ok());
}

#[then("the read succeeds")]
async fn then_read_ok(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the chunk is flagged for background re-wrapping to epoch 3")]
async fn then_flagged_rewrap(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: DEK wrapping ===

#[then("the DEK is wrapped with the system KEK")]
async fn then_wrapped_dek(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the wrapped DEK is stored in the chunk envelope")]
async fn then_stored_envelope(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the plaintext DEK is held only in memory, never persisted")]
async fn then_dek_in_memory(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: KEK rotation extra steps ===

#[then(regex = r#"^new chunks use system DEKs wrapped with "(\S+)"$"#)]
async fn then_new_chunks_epoch(w: &mut KisekiWorld, _kek: String) {
    let epoch = w.key_store.current_epoch().await.unwrap();
    assert!(epoch.0 >= 2);
}

#[then(regex = r#"^background re-wrapping migrates epoch (\d+) DEK wrappings to epoch (\d+)$"#)]
async fn then_migration(_w: &mut KisekiWorld, _from: u64, _to: u64) {
    panic!("not yet implemented");
}

#[then("the rotation event is recorded in the audit log")]
async fn then_rotation_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Tenant KEK wrap details ===

#[then(regex = r#"^"(\S+)" can: unwrap "(\S+)" with their KEK .+$"#)]
async fn then_can_unwrap(_w: &mut KisekiWorld, _t: String, _d: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^the system can: unwrap "(\S+)" with system KEK .+$"#)]
async fn then_system_unwrap(_w: &mut KisekiWorld, _d: String) {
    panic!("not yet implemented");
}

#[then("both wrappings coexist in the envelope")]
async fn then_coexist(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Tenant without KMS ===

#[given(regex = r#"^a new tenant "(\S+)" has been created but has not configured a KMS$"#)]
async fn given_no_kms(w: &mut KisekiWorld, t: String) {
    w.ensure_tenant(&t);
}

#[when(regex = r#"^"(\S+)" attempts to read a chunk$"#)]
async fn when_read_no_kms(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^the read fails with "tenant KMS not configured" error$"#)]
async fn then_no_kms_error(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// "no data is returned" step is in auth.rs

#[then("the access attempt is recorded in the audit log")]
async fn then_access_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Tenant KEK rotation ===

#[given(regex = r#"^"(\S+)" tenant KEK "(\S+)" is epoch (\d+)$"#)]
async fn given_tenant_epoch(_w: &mut KisekiWorld, _t: String, _k: String, _e: u64) {
    panic!("not yet implemented");
}

#[when("the tenant admin rotates the tenant KEK")]
async fn when_tenant_rotate(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^a new KEK "(\S+)" is generated \(epoch (\d+)\) in the tenant KMS$"#)]
async fn then_new_kek(_w: &mut KisekiWorld, _k: String, _e: u64) {
    panic!("not yet implemented");
}

#[then(regex = r#"^new chunks get system DEK wrappings under epoch (\d+) tenant KEK$"#)]
async fn then_new_wrapping(_w: &mut KisekiWorld, _e: u64) {
    panic!("not yet implemented");
}

#[then(regex = r#"^existing chunks retain epoch (\d+) tenant KEK wrapping$"#)]
async fn then_retain_tenant(w: &mut KisekiWorld, epoch: u64) {
    assert!(w.key_store.fetch_master_key(KeyEpoch(epoch)).await.is_ok());
}

#[then(regex = r#"^background re-wrapping migrates epoch (\d+) wrappings to epoch (\d+)$"#)]
async fn then_bg_rewrap(_w: &mut KisekiWorld, _from: u64, _to: u64) {
    panic!("not yet implemented");
}

// "both epochs are valid during migration" is matched by "both epochs are valid.*" regex above

#[then("old data remains accessible throughout rotation")]
async fn then_accessible(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the rotation event is recorded in the audit log (tenant export)")]
async fn then_tenant_rotation_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Full re-encryption ===

#[given(regex = r#"^"(\S+)" suspects key compromise of "(\S+)"$"#)]
async fn given_compromise(_w: &mut KisekiWorld, _t: String, _k: String) {
    panic!("not yet implemented");
}

#[when("the tenant admin triggers full re-encryption")]
async fn when_reencrypt(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^all chunks referenced by "(\S+)" are:$"#)]
async fn then_reencrypted(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("old system DEKs for affected chunks are destroyed")]
async fn then_old_deks_destroyed(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("old tenant KEK wrappings are destroyed")]
async fn then_old_wrappings_destroyed(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the operation runs in background with progress tracking")]
async fn then_bg_progress(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the re-encryption event is recorded in the audit log")]
async fn then_reencrypt_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Crypto-shred ===

#[given(regex = r#"^"(\S+)" has chunks \[([^\]]+)\] with refcounts \[([^\]]+)\]$"#)]
async fn given_chunks_with_refs(_w: &mut KisekiWorld, _t: String, _chunks: String, _refs: String) {
    panic!("not yet implemented");
}

#[when(regex = r#"^the tenant admin performs crypto-shred for "(\S+)"$"#)]
async fn when_crypto_shred(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^tenant KEK "(\S+)" is destroyed in the tenant KMS$"#)]
async fn then_kek_destroyed(_w: &mut KisekiWorld, _k: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^all tenant KEK wrappings for "(\S+)" become invalid$"#)]
async fn then_wrappings_invalid(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("system DEKs can no longer be unwrapped via tenant path")]
async fn then_no_unwrap(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("chunks remain on storage as system-encrypted ciphertext")]
async fn then_chunks_remain(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^refcounts for "(\S+)"'s references are decremented$"#)]
async fn then_refs_decremented(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("the crypto-shred event is recorded in the audit log (system + tenant export)")]
async fn then_shred_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Crypto-shred with retention ===

#[given(regex = r#"^a retention hold "(\S+)" is active on "(\S+)" namespace "(\S+)"$"#)]
async fn given_retention_ns(_w: &mut KisekiWorld, _hold: String, _t: String, _ns: String) {
    panic!("not yet implemented");
}

#[when(regex = r#"^crypto-shred is performed for "(\S+)"$"#)]
async fn when_crypto_shred2(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("tenant KEK is destroyed (data unreadable)")]
async fn then_kek_gone(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("chunks with refcount 0 are NOT physically deleted (hold active)")]
async fn then_hold_blocks(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("system-encrypted ciphertext is retained until hold expires")]
async fn then_ciphertext_retained(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the hold-preserving-after-shred state is recorded in the audit log")]
async fn then_hold_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Cross-tenant crypto-shred ===

#[given(regex = r#"^chunk "(\S+)" has refcount (\d+) \(([^)]+)\)$"#)]
async fn given_shared_chunk(_w: &mut KisekiWorld, _chunk: String, _rc: u64, _tenants: String) {
    panic!("not yet implemented");
}

// "X performs crypto-shred" step is defined in chunk.rs to avoid ambiguity

#[then(regex = r#"^"(\S+)"'s KEK wrapping for "(\S+)" is invalidated$"#)]
async fn then_kek_invalid(_w: &mut KisekiWorld, _t: String, _chunk: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^"(\S+)"'s KEK wrapping remains valid$"#)]
async fn then_kek_valid(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^"(\S+)" can still read "(\S+)"$"#)]
async fn then_can_read(_w: &mut KisekiWorld, _t: String, _chunk: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^"(\S+)" refcount decrements to (\d+)$"#)]
async fn then_rc_decremented(_w: &mut KisekiWorld, _chunk: String, _rc: u64) {
    panic!("not yet implemented");
}

#[then(regex = r#"^"(\S+)" is NOT eligible for GC \(refcount > 0\)$"#)]
async fn then_not_gc(_w: &mut KisekiWorld, _chunk: String) {
    panic!("not yet implemented");
}

// === Scenario: KMS unreachable (cached) ===

#[given(regex = r#"^"(\S+)" KMS is unreachable$"#)]
async fn given_kms_unreachable(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[given(regex = r#"^cached tenant KEK material has a TTL of (\d+) seconds$"#)]
async fn given_cache_ttl(_w: &mut KisekiWorld, _ttl: u64) {
    panic!("not yet implemented");
}

#[when(regex = r#"^a read request arrives for "(\S+)" data within the cache window$"#)]
async fn when_read_cached(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("the cached KEK is used to unwrap the system DEK")]
async fn then_cached_unwrap(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^a warning is logged.*$"#)]
async fn then_warning_logged(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: KMS unreachable (expired) ===

#[given(regex = r#"^"(\S+)" KMS has been unreachable for (\d+) seconds$"#)]
async fn given_kms_unreachable_long(_w: &mut KisekiWorld, _t: String, _s: u64) {
    panic!("not yet implemented");
}

#[given(regex = r#"^the cached KEK TTL of (\d+) seconds has expired$"#)]
async fn given_ttl_expired(_w: &mut KisekiWorld, _ttl: u64) {
    panic!("not yet implemented");
}

#[when(regex = r#"^a read request arrives for "(\S+)" data$"#)]
async fn when_read_expired(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^the read fails with "tenant KMS unavailable, key cache expired" error$"#)]
async fn then_cache_expired_error(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the tenant admin and cluster admin are alerted")]
async fn then_both_alerted(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("no stale key material is used beyond the TTL")]
async fn then_no_stale(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Federated KMS ===

#[given(regex = r#"^"(\S+)" has data at (\S+) and (\S+)$"#)]
async fn given_multi_site(_w: &mut KisekiWorld, _t: String, _s1: String, _s2: String) {
    panic!("not yet implemented");
}

#[given(regex = r#"^tenant KMS is at "(\S+)"$"#)]
async fn given_kms_addr(_w: &mut KisekiWorld, _addr: String) {
    panic!("not yet implemented");
}

#[when(regex = r#"^(\S+) needs to decrypt "(\S+)" data$"#)]
async fn when_site_decrypt(_w: &mut KisekiWorld, _site: String, _t: String) {
    panic!("not yet implemented");
}

#[then(regex = r#"^(\S+) contacts "(\S+)" over encrypted channel$"#)]
async fn then_contacts_kms(_w: &mut KisekiWorld, _site: String, _addr: String) {
    panic!("not yet implemented");
}

#[then("obtains tenant KEK wrapping for the requested system DEK")]
async fn then_obtains_kek(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("decryption proceeds using the unwrapped DEK")]
async fn then_decrypt_proceeds(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the KMS connection is authenticated and encrypted end-to-end")]
async fn then_e2e_encrypted(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Key audit ===

#[given("any key event occurs:")]
async fn given_key_event(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the event is recorded in the audit log with:")]
async fn then_event_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the event is included in the tenant audit export (if tenant-scoped)")]
async fn then_tenant_export(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("keys themselves are NEVER recorded in the audit log")]
async fn then_no_keys_in_audit(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: KMS permanently lost ===

#[given(regex = r#"^"(\S+)" KMS infrastructure is destroyed$"#)]
async fn given_kms_destroyed(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[given(regex = r#"^"(\S+)" has no KMS backups$"#)]
async fn given_no_backups(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[when(regex = r#"^any operation requiring "(\S+)" tenant KEK is attempted$"#)]
async fn when_kek_op(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[then("the operation fails permanently")]
async fn then_perm_fail(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^all "(\S+)" data is unreadable.*$"#)]
async fn then_unreadable(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

// "the cluster admin is alerted" step is defined in chunk.rs

#[then("Kiseki does not provide key escrow or recovery")]
async fn then_no_escrow(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then(regex = r#"^the loss is documented as tenant responsibility per I-K11$"#)]
async fn then_tenant_responsibility(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: System key manager failure ===

#[given("the system key manager is an internal HA Kiseki service")]
async fn given_ha_km(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[given("the system key manager loses quorum")]
async fn given_km_quorum_loss(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[when("a new chunk write requires a system DEK")]
async fn when_dek_required(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the write fails with retriable error")]
async fn then_retriable(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("cached system DEKs for reads may still work within cache TTL")]
async fn then_cached_reads(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the cluster admin is alerted immediately (highest severity)")]
async fn then_alert_high(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("no data is written without proper system encryption")]
async fn then_no_unencrypted(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("this is a cluster-wide write outage until quorum is restored")]
async fn then_write_outage(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Concurrent rotation and shred ===

#[given(regex = r#"^"(\S+)" tenant admin initiates key rotation$"#)]
async fn given_rotation_initiated(_w: &mut KisekiWorld, _t: String) {
    panic!("not yet implemented");
}

#[given("simultaneously another admin initiates crypto-shred")]
async fn given_concurrent_shred(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("exactly one operation succeeds (serialized via tenant KMS)")]
async fn then_serialized(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("if rotation wins: rotation completes, then shred can proceed with new KEK")]
async fn then_rotation_wins(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("if shred wins: KEK is destroyed, rotation is moot")]
async fn then_shred_wins(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

#[then("the outcome is deterministic and audited")]
async fn then_deterministic(_w: &mut KisekiWorld) {
    panic!("not yet implemented");
}

// === Scenario: Epoch mismatch extra ===

#[then(regex = r#"^and unwraps the system DEK using epoch (\d+) material$"#)]
async fn then_unwrap_epoch(w: &mut KisekiWorld, epoch: u64) {
    assert!(w.key_store.fetch_master_key(KeyEpoch(epoch)).await.is_ok());
}
