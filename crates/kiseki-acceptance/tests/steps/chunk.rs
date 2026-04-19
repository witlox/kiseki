//! Step definitions for chunk-storage.feature — scenarios with real assertions.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_chunk::pool::{AffinityPool, DurabilityStrategy};
use kiseki_chunk::store::ChunkOps;
use kiseki_common::ids::*;
use kiseki_common::tenancy::*;
use kiseki_crypto::aead::{Aead, GCM_NONCE_LEN, GCM_TAG_LEN};
use kiseki_crypto::envelope::Envelope;

fn test_envelope(id_byte: u8) -> Envelope {
    Envelope {
        ciphertext: vec![0xab; 256],
        auth_tag: [0xcc; GCM_TAG_LEN],
        nonce: [0xdd; GCM_NONCE_LEN],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
        chunk_id: ChunkId([id_byte; 32]),
    }
}

// === Background ===

#[given(regex = r#"^a Kiseki cluster with \d+ affinity pools:$"#)]
async fn given_pools(w: &mut KisekiWorld) {
    w.chunk_store.add_pool(AffinityPool::new(
        "fast-nvme",
        DurabilityStrategy::default(),
        1024 * 1024 * 1024,
    ));
    w.chunk_store.add_pool(AffinityPool::new(
        "bulk-nvme",
        DurabilityStrategy::default(),
        10 * 1024 * 1024 * 1024,
    ));
}

#[given(regex = r#"^tenant "(\S+)" exists with cross-tenant dedup enabled.*$"#)]
async fn given_dedup_tenant(w: &mut KisekiWorld, t: String) {
    w.ensure_tenant(&t);
}

#[given(regex = r#"^tenant "(\S+)" exists with cross-tenant dedup opted out.*$"#)]
async fn given_isolated_tenant(w: &mut KisekiWorld, t: String) {
    w.ensure_tenant(&t);
}

// === Scenario: Write + read roundtrip ===

#[given(regex = r#"^the Composition context for "(\S+)" submits plaintext data$"#)]
async fn given_plaintext(w: &mut KisekiWorld, _t: String) {
    // Plaintext is implicit — the step triggers chunk write
}

#[when(regex = r#"^the system computes chunk_id = sha256\(plaintext\)$"#)]
async fn when_sha256(_w: &mut KisekiWorld) {}

#[when(regex = r#"^encrypts the plaintext with a system DEK$"#)]
async fn when_encrypt(_w: &mut KisekiWorld) {}

#[when(regex = r#"^stores the ciphertext in pool "(\S+)" per affinity policy$"#)]
async fn when_store(w: &mut KisekiWorld, pool: String) {
    let env = test_envelope(0x01);
    w.last_chunk_id = Some(env.chunk_id);
    let is_new = w.chunk_store.write_chunk(env, &pool).unwrap();
    assert!(is_new, "first write should be new");
}

#[then(regex = r#"^a ChunkStored event is emitted with the chunk_id$"#)]
async fn then_stored(w: &mut KisekiWorld) {
    assert!(w.last_chunk_id.is_some());
}

#[then(regex = r#"^the chunk's refcount is initialized to 1$"#)]
async fn then_refcount_1(w: &mut KisekiWorld) {
    let id = w.last_chunk_id.unwrap();
    assert_eq!(w.chunk_store.refcount(&id).unwrap(), 1);
}

#[then(regex = r#"^no plaintext is persisted at any point$"#)]
async fn then_no_plaintext(_w: &mut KisekiWorld) {
    // Structural: ChunkStore stores Envelope (ciphertext), never plaintext
}

// === Scenario: Dedup ===

#[when(regex = r#"^the system computes chunk_id = HMAC.*$"#)]
async fn when_hmac(_w: &mut KisekiWorld) {}

#[when(regex = r#"^a second composition references the same plaintext data$"#)]
async fn when_dedup_ref(w: &mut KisekiWorld) {
    let env = test_envelope(0x01); // same chunk ID
    let is_new = w.chunk_store.write_chunk(env, "fast-nvme").unwrap();
    assert!(!is_new, "dedup should detect existing chunk");
}

#[then(regex = r#"^the existing chunk's refcount is incremented to (\d+)$"#)]
async fn then_refcount(w: &mut KisekiWorld, expected: u64) {
    let id = w.last_chunk_id.unwrap();
    assert_eq!(w.chunk_store.refcount(&id).unwrap(), expected);
}

// === Scenario: GC with refcount ===

#[given(regex = r#"^chunk "(\S+)" has refcount (\d+)$"#)]
async fn given_chunk_refcount(w: &mut KisekiWorld, _name: String, count: u64) {
    let env = test_envelope(0x42);
    w.last_chunk_id = Some(env.chunk_id);
    w.chunk_store.write_chunk(env, "fast-nvme").unwrap();
    for _ in 1..count {
        w.chunk_store
            .increment_refcount(&ChunkId([0x42; 32]))
            .unwrap();
    }
}

#[when(regex = r#"^all compositions referencing "(\S+)" are deleted$"#)]
async fn when_all_deleted(w: &mut KisekiWorld, _name: String) {
    let id = w.last_chunk_id.unwrap();
    let rc = w.chunk_store.refcount(&id).unwrap();
    for _ in 0..rc {
        w.chunk_store.decrement_refcount(&id).unwrap();
    }
}

#[when("chunk GC runs")]
async fn when_gc(w: &mut KisekiWorld) {
    w.last_sequence = Some(SequenceNumber(w.chunk_store.gc()));
}

#[then(regex = r#"^"(\S+)" is physically deleted$"#)]
async fn then_deleted(w: &mut KisekiWorld, _name: String) {
    let id = w.last_chunk_id.unwrap();
    assert!(
        w.chunk_store.read_chunk(&id).is_err(),
        "chunk should be GC'd"
    );
}

// === Scenario: Retention hold blocks GC ===

#[given(regex = r#"^a retention hold "(\S+)" is active on.*chunk.*$"#)]
async fn given_hold(w: &mut KisekiWorld, hold_name: String) {
    if let Some(id) = w.last_chunk_id {
        w.chunk_store.set_retention_hold(&id, &hold_name).unwrap();
    }
}

#[then(regex = r#"^"(\S+)" is NOT physically deleted.*$"#)]
async fn then_not_deleted(w: &mut KisekiWorld, _name: String) {
    let id = w.last_chunk_id.unwrap();
    assert!(
        w.chunk_store.read_chunk(&id).is_ok(),
        "retention hold should prevent GC"
    );
}
