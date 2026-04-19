//! Step definitions for composition.feature — scenarios with real assertions.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_common::ids::*;
use kiseki_composition::composition::{CompositionOps, CompositionStore};
use kiseki_composition::error::CompositionError;
use kiseki_composition::namespace::Namespace;

// === Scenario: Create composition ===

#[given(regex = r#"^a namespace "(\S+)" in shard "(\S+)" owned by "(\S+)"$"#)]
async fn given_ns(w: &mut KisekiWorld, ns: String, shard: String, tenant: String) {
    let shard_id = w.ensure_shard(&shard);
    let tenant_id = w.ensure_tenant(&tenant);
    let ns_id = w.ensure_namespace(&ns, &shard);
}

#[when(regex = r#"^a composition is created in namespace "(\S+)"$"#)]
async fn when_create(w: &mut KisekiWorld, ns: String) {
    let ns_id = *w.namespace_ids.get(&ns).unwrap();
    match w.comp_store.create(ns_id, vec![ChunkId([0x01; 32])], 1024) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[then("the composition is created successfully")]
async fn then_created(w: &mut KisekiWorld) {
    assert!(w.last_composition_id.is_some(), "error: {:?}", w.last_error);
}

// === Scenario: Delete ===

#[when(regex = r#"^the composition is deleted$"#)]
async fn when_delete(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        match w.comp_store.delete(id) {
            Ok(()) => w.last_error = None,
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[then("the composition no longer exists")]
async fn then_gone(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        assert!(w.comp_store.get(id).is_err());
    }
}

// === Scenario: Cross-shard rename EXDEV ===

#[given(regex = r#"^a namespace "(\S+)" on a different shard$"#)]
async fn given_other_ns(w: &mut KisekiWorld, ns: String) {
    let other_shard = ShardId(uuid::Uuid::new_v4());
    let tenant_id = w.ensure_tenant("org-pharma");
    let ns_id = NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        ns.as_bytes(),
    ));
    w.comp_store.add_namespace(Namespace {
        id: ns_id,
        tenant_id,
        shard_id: other_shard,
        read_only: false,
    });
    w.namespace_ids.insert(ns, ns_id);
}

#[when(regex = r#"^the composition is renamed to namespace "(\S+)"$"#)]
async fn when_rename(w: &mut KisekiWorld, target_ns: String) {
    if let Some(id) = w.last_composition_id {
        let ns_id = *w.namespace_ids.get(&target_ns).unwrap();
        match w.comp_store.rename(id, ns_id) {
            Ok(()) => w.last_error = None,
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[then(regex = r#"^the rename returns EXDEV$"#)]
async fn then_exdev(w: &mut KisekiWorld) {
    assert!(
        w.last_error
            .as_ref()
            .is_some_and(|e| e.contains("cross-shard")),
        "expected EXDEV, got: {:?}",
        w.last_error
    );
}

// === Scenario: Read-only namespace ===

#[given(regex = r#"^namespace "(\S+)" is marked read-only$"#)]
async fn given_readonly(w: &mut KisekiWorld, ns: String) {
    let shard_id = w.ensure_shard("shard-alpha");
    let tenant_id = w.ensure_tenant("org-pharma");
    let ns_id = NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        ns.as_bytes(),
    ));
    w.comp_store.add_namespace(Namespace {
        id: ns_id,
        tenant_id,
        shard_id,
        read_only: true,
    });
    w.namespace_ids.insert(ns, ns_id);
}

#[then("the create is rejected with read-only error")]
async fn then_readonly(w: &mut KisekiWorld) {
    assert!(
        w.last_error
            .as_ref()
            .is_some_and(|e| e.contains("read-only")),
        "expected read-only error, got: {:?}",
        w.last_error
    );
}

// === Scenario: Versioning ===

#[when("the composition is updated with new chunks")]
async fn when_update(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        match w
            .comp_store
            .update(id, vec![ChunkId([0x02; 32]), ChunkId([0x03; 32])], 2048)
        {
            Ok(v) => {
                w.last_epoch = Some(v);
                w.last_error = None;
            }
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[then(regex = r#"^the version is incremented to (\d+)$"#)]
async fn then_version(w: &mut KisekiWorld, expected: u64) {
    assert_eq!(w.last_epoch, Some(expected));
}

// Background steps shared with other features
#[given(regex = r#"^a Kiseki cluster with tenant "(\S+)"$"#)]
async fn given_cluster_tenant(w: &mut KisekiWorld, tenant: String) {
    w.ensure_tenant(&tenant);
}

#[given(regex = r#"^namespace "(\S+)" in shard "(\S+)"$"#)]
async fn given_ns_in_shard(w: &mut KisekiWorld, ns: String, shard: String) {
    w.ensure_namespace(&ns, &shard);
}

#[given(regex = r#"^tenant KEK "(\S+)" is active$"#)]
async fn given_tenant_kek_active(_w: &mut KisekiWorld, _kek: String) {
    // KEK setup is a no-op in the in-memory test harness — the crypto
    // layer is exercised by kiseki-crypto unit/property tests.
}

// === Scenario: Create composition via protocol gateway ===

#[given(regex = r#"^the protocol gateway receives an NFS CREATE for "([^"]+)"$"#)]
async fn given_pgw_nfs_create(w: &mut KisekiWorld, _path: String) {
    // Setup: ensure the namespace from the background exists.
    // The When step processes the actual create.
    w.ensure_namespace("trials", "shard-trials-1");
}

#[given(regex = r#"^the protocol gateway receives a CREATE for a (\d+)-byte file$"#)]
async fn given_pgw_create_small(w: &mut KisekiWorld, _size: u64) {
    // Inline data scenario — namespace from background is sufficient.
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Append / overwrite existing composition ===

#[given(regex = r#"^composition "([^"]+)" exists with chunks \[([^\]]+)\]$"#)]
async fn given_comp_with_chunks(w: &mut KisekiWorld, name: String, chunks_str: String) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    let chunk_count = chunks_str.split(',').count();
    let chunks: Vec<ChunkId> = (0..chunk_count)
        .map(|i| ChunkId([(i as u8) + 1; 32]))
        .collect();
    let size = chunk_count as u64 * 64 * 1024 * 1024;
    match w.comp_store.create(ns_id, chunks, size) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

// === Scenario: S3 multipart upload ===

#[given("the protocol gateway receives an S3 CreateMultipartUpload")]
async fn given_pgw_s3_multipart(w: &mut KisekiWorld) {
    w.ensure_namespace("trials", "shard-trials-1");
}

#[given(regex = r#"^a multipart upload is in progress with chunks \[([^\]]+)\] stored$"#)]
async fn given_multipart_in_progress(w: &mut KisekiWorld, _chunks: String) {
    // Multipart is not yet modelled in the in-memory CompositionStore.
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Delete composition ===

#[given(regex = r#"^composition "([^"]+)" references chunks \[([^\]]+)\]$"#)]
async fn given_comp_refs_chunks(w: &mut KisekiWorld, _name: String, chunks_str: String) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    let chunk_count = chunks_str.split(',').count();
    let chunks: Vec<ChunkId> = (0..chunk_count)
        .map(|i| ChunkId([(i as u8) + 5; 32]))
        .collect();
    let size = chunk_count as u64 * 64 * 1024 * 1024;
    match w.comp_store.create(ns_id, chunks, size) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

// === Scenario: Delete with object versioning ===

#[given(regex = r#"^namespace "(\S+)" has object versioning enabled$"#)]
async fn given_ns_versioning(w: &mut KisekiWorld, ns: String) {
    // Versioning flag — ensure the namespace exists; versioning is
    // tracked at the namespace/store level, not modelled in-memory yet.
    w.ensure_namespace(&ns, "shard-trials-1");
}

// === Scenario: Intra-tenant dedup ===

#[given(
    regex = r#"^"([^"]+)" writes file A with plaintext P \(chunk_id = sha256\(P\) = "([^"]+)"\)$"#
)]
async fn given_tenant_writes_file_a(w: &mut KisekiWorld, tenant: String, _chunk_id: String) {
    w.ensure_tenant(&tenant);
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Cross-tenant dedup ===

#[given(regex = r#"^"([^"]+)" has chunk "([^"]+)" \(refcount (\d+)\)$"#)]
async fn given_tenant_has_chunk_refcount(
    w: &mut KisekiWorld,
    tenant: String,
    _chunk_name: String,
    _refcount: u64,
) {
    w.ensure_tenant(&tenant);
}

// === Scenario: No cross-tenant dedup (HMAC) ===

#[given(regex = r#"^"([^"]+)" \(HMAC chunk IDs\) writes plaintext P$"#)]
async fn given_hmac_tenant_writes(w: &mut KisekiWorld, tenant: String) {
    w.ensure_tenant(&tenant);
}

// === Scenario: Create namespace ===

#[given(regex = r#"^tenant admin for "([^"]+)" requests new namespace "([^"]+)"$"#)]
async fn given_tenant_admin_requests_ns(w: &mut KisekiWorld, tenant: String, _ns: String) {
    w.ensure_tenant(&tenant);
}

// === Scenario: Namespace inherits compliance tags ===

#[given(regex = r#"^org "([^"]+)" has compliance tags \[([^\]]+)\]$"#)]
async fn given_org_compliance_tags(w: &mut KisekiWorld, org: String, _tags: String) {
    w.ensure_tenant(&org);
}

// === Scenario: Chunk write fails during composition create ===

#[given("the Composition context is creating a new composition")]
async fn given_comp_ctx_creating(w: &mut KisekiWorld) {
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Delta commit fails after chunk write succeeds ===

#[given(regex = r#"^chunk c(\d+) was successfully written \(refcount (\d+)\)$"#)]
async fn given_chunk_written_with_refcount(w: &mut KisekiWorld, chunk_num: u64, _refcount: u64) {
    let chunk_id = ChunkId([chunk_num as u8; 32]);
    w.last_chunk_id = Some(chunk_id);
}

// === Scenario: Cross-shard rename returns EXDEV ===

#[given(regex = r#"^composition "([^"]+)" exists in namespace "([^"]+)" \((\S+)\)$"#)]
async fn given_comp_in_ns_shard(w: &mut KisekiWorld, _name: String, ns: String, shard: String) {
    let ns_id = w.ensure_namespace(&ns, &shard);
    let chunks = vec![ChunkId([0x10; 32])];
    match w.comp_store.create(ns_id, chunks, 4096) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

// === Scenario: Collective checkpoint announcement ===

#[given(regex = r#"^workload "([^"]+)" is in phase "([^"]+)" with profile (\S+)$"#)]
async fn given_workload_phase_profile(
    w: &mut KisekiWorld,
    _workload: String,
    _phase: String,
    _profile: String,
) {
    // Advisory setup — workload phase/profile is modelled in the advisory table.
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Retention-intent at multipart finalize ===

#[given(regex = r#"^a multipart upload for composition "([^"]+)" is in progress$"#)]
async fn given_multipart_for_comp(w: &mut KisekiWorld, _name: String) {
    // Multipart is not yet modelled in the in-memory CompositionStore.
    w.ensure_namespace("trials", "shard-trials-1");
}

// === Scenario: Caller-scoped refcount activity telemetry ===

#[given(
    regex = r#"^workload "([^"]+)" performs rapid creates/updates on compositions in namespace "([^"]+)"$"#
)]
async fn given_workload_rapid_writes(w: &mut KisekiWorld, _workload: String, ns: String) {
    w.ensure_namespace(&ns, "shard-trials-1");
}

// === Scenario: Hint cannot enable cross-namespace composition creation ===

#[given(regex = r#"^workload "([^"]+)" is authorised for namespace "([^"]+)" only$"#)]
async fn given_workload_authorised_ns(w: &mut KisekiWorld, _workload: String, ns: String) {
    w.ensure_namespace(&ns, "shard-trials-1");
}
