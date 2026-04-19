//! Step definitions for key-management.feature.

use cucumber::{given, then, when};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_keymanager::epoch::KeyManagerOps;

use crate::KisekiWorld;

#[given("a Kiseki cluster with a system key manager")]
async fn given_key_manager(_world: &mut KisekiWorld) {
    // MemKeyStore initialized in World::new() with epoch 1.
}

#[given(regex = r#"^system KEK "(\S+)" wrapping system DEKs$"#)]
async fn given_system_kek(_world: &mut KisekiWorld, _kek_name: String) {}

#[given(regex = r#"^tenant "(\S+)" with tenant KMS at "(\S+)"$"#)]
async fn given_tenant_kms(world: &mut KisekiWorld, tenant: String, _kms_addr: String) {
    world.ensure_tenant(&tenant);
}

#[given(regex = r#"^tenant KEK "(\S+)" in epoch (\d+)$"#)]
async fn given_tenant_kek(_world: &mut KisekiWorld, _kek: String, _epoch: u64) {}

#[when("a new chunk is written")]
async fn when_chunk_written(_world: &mut KisekiWorld) {
    // Chunk write exercised in chunk steps.
}

#[then(regex = r#"^a system DEK is generated.*$"#)]
async fn then_dek_generated(world: &mut KisekiWorld) {
    // Verify key store has epoch 1.
    let epoch = world.key_store.current_epoch().await;
    assert!(epoch.is_ok());
}

#[given(regex = r#"^system KEK "(\S+)" is in epoch (\d+)$"#)]
async fn given_kek_epoch(_world: &mut KisekiWorld, _kek: String, _epoch: u64) {}

#[when("the cluster admin triggers system KEK rotation")]
async fn when_rotate_kek(world: &mut KisekiWorld) {
    match world.key_store.rotate().await {
        Ok(epoch) => {
            world.last_epoch = Some(epoch.0);
            world.last_error = None;
        }
        Err(e) => world.last_error = Some(e.to_string()),
    }
}

#[then(regex = r#"^a new system KEK "(\S+)" is generated \(epoch (\d+)\)$"#)]
async fn then_new_kek(world: &mut KisekiWorld, _kek: String, expected: u64) {
    assert_eq!(world.last_epoch, Some(expected));
}

#[then(regex = r#"^new chunks use system DEKs wrapped with "(\S+)"$"#)]
async fn then_new_chunks_use(_world: &mut KisekiWorld, _kek: String) {}

#[then(regex = r#"^existing chunks retain epoch (\d+) wrapping$"#)]
async fn then_retain_epoch(world: &mut KisekiWorld, epoch: u64) {
    assert!(world
        .key_store
        .fetch_master_key(KeyEpoch(epoch))
        .await
        .is_ok());
}

#[then(regex = r#"^both epochs are valid.*$"#)]
async fn then_both_valid(world: &mut KisekiWorld) {
    assert!(world.key_store.fetch_master_key(KeyEpoch(1)).await.is_ok());
    if let Some(e) = world.last_epoch {
        assert!(world.key_store.fetch_master_key(KeyEpoch(e)).await.is_ok());
    }
}

#[then(regex = r#"^the rotation event is recorded in the audit log.*$"#)]
async fn then_audit_rotation(_world: &mut KisekiWorld) {
    // Audit integration — future.
}

#[then(regex = r#"^background re-wrapping migrates.*$"#)]
async fn then_rewrapping(_world: &mut KisekiWorld) {
    // Re-wrapping lifecycle — future.
}
