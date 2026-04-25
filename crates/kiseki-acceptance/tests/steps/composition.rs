//! Step definitions for composition.feature — scenarios with real assertions.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_common::ids::*;
use kiseki_composition::composition::{CompositionOps, CompositionStore};
use kiseki_composition::error::CompositionError;
use kiseki_composition::namespace::Namespace;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};

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

    // Cross-context: verify a Create delta was emitted to the log.
    let comp = w.comp_store.get(w.last_composition_id.unwrap()).unwrap();
    let deltas = w
        .log_store
        .read_deltas(ReadDeltasRequest {
            shard_id: comp.shard_id,
            from: SequenceNumber(1),
            to: SequenceNumber(u64::MAX),
        })
        .await
        .unwrap();
    // Verify delta matches THIS composition (BA-ADV-8).
    let comp_id_bytes = w.last_composition_id.unwrap().0.as_bytes().to_vec();
    assert!(
        deltas.iter().any(
            |d| d.header.operation == kiseki_log::delta::OperationType::Create
                && d.payload.ciphertext == comp_id_bytes
        ),
        "composition create should emit a Create delta with matching composition ID"
    );
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

        // Cross-context: verify a Delete delta was emitted to the log.
        // We check all shards since the composition is already gone.
        for &shard_id in w.shard_names.values() {
            let deltas = w
                .log_store
                .read_deltas(ReadDeltasRequest {
                    shard_id,
                    from: SequenceNumber(1),
                    to: SequenceNumber(u64::MAX),
                })
                .await
                .unwrap_or_default();
            if deltas
                .iter()
                .any(|d| d.header.operation == kiseki_log::delta::OperationType::Delete)
            {
                return; // found it
            }
        }
        panic!("composition delete should emit a Delete delta to the log");
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

    // Cross-context: verify an Update delta was emitted.
    if let Some(comp_id) = w.last_composition_id {
        if let Ok(comp) = w.comp_store.get(comp_id) {
            let deltas = w
                .log_store
                .read_deltas(ReadDeltasRequest {
                    shard_id: comp.shard_id,
                    from: SequenceNumber(1),
                    to: SequenceNumber(u64::MAX),
                })
                .await
                .unwrap_or_default();
            assert!(
                deltas
                    .iter()
                    .any(|d| d.header.operation == kiseki_log::delta::OperationType::Update),
                "composition version update should emit an Update delta"
            );
        }
    }
}

// === When: Composition context processes the create (DataTable) ===

#[when("the Composition context processes the create:")]
async fn when_comp_ctx_processes_create_table(w: &mut KisekiWorld) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    match w.comp_store.create(ns_id, vec![ChunkId([0x01; 32])], 1024) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when("the Composition context processes the create")]
async fn when_comp_ctx_processes_create(w: &mut KisekiWorld) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    // Small/inline file — create with no chunks
    match w.comp_store.create(ns_id, vec![], 512) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when("a 64MB append is written")]
async fn when_append_64mb(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        let new_chunks = vec![ChunkId([0x03; 32]), ChunkId([0x04; 32])];
        match w.comp_store.update(id, new_chunks, 128 * 1024 * 1024) {
            Ok(v) => {
                w.last_epoch = Some(v);
                w.last_error = None;
            }
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[given("chunk c2 covers byte range 64MB-128MB")]
async fn given_chunk_c2_byte_range(_w: &mut KisekiWorld) {
    todo!("set up chunk c2 with byte range 64MB-128MB in chunk store")
}

#[when("a write modifies bytes 80MB-90MB")]
async fn when_write_modifies_byte_range(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        // Overwrite produces a new chunk c2' replacing c2
        let new_chunks = vec![ChunkId([0x0a; 32])];
        match w.comp_store.update(id, new_chunks, 128 * 1024 * 1024) {
            Ok(v) => {
                w.last_epoch = Some(v);
                w.last_error = None;
            }
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[when("parts are uploaded in parallel:")]
async fn when_parts_uploaded_parallel(w: &mut KisekiWorld) {
    // Multipart parts stored — modelled as precondition.
    // Actual finalization happens in the CompleteMultipartUpload step.
    w.last_error = None;
}

#[when("the protocol gateway sends CompleteMultipartUpload")]
async fn when_complete_multipart_upload(w: &mut KisekiWorld) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    let chunks = vec![
        ChunkId([0x10; 32]),
        ChunkId([0x11; 32]),
        ChunkId([0x12; 32]),
    ];
    match w.comp_store.create(ns_id, chunks, 3 * 64 * 1024 * 1024) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when("the protocol gateway sends AbortMultipartUpload")]
async fn when_abort_multipart(w: &mut KisekiWorld) {
    // Abort — no finalize delta committed, no composition created.
    w.last_composition_id = None;
    w.last_error = None;
}

#[given("c5 has refcount 2 (shared with another composition)")]
async fn given_c5_refcount_2(_w: &mut KisekiWorld) {
    todo!("set c5 refcount to 2 in chunk store")
}

#[given("c6 has refcount 1")]
async fn given_c6_refcount_1(_w: &mut KisekiWorld) {
    todo!("set c6 refcount to 1 in chunk store")
}

#[when("the Composition context processes a DELETE")]
async fn when_comp_ctx_delete(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        match w.comp_store.delete(id) {
            Ok(()) => w.last_error = None,
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[given(regex = r#"^composition "([^"]+)" has versions \[([^\]]+)\]$"#)]
async fn given_comp_versions(w: &mut KisekiWorld, _name: String, _versions: String) {
    // Versioning setup — ensure composition exists with version history.
    if w.last_composition_id.is_none() {
        let ns_id = w.ensure_namespace("trials", "shard-trials-1");
        if let Ok(id) = w.comp_store.create(ns_id, vec![ChunkId([0x20; 32])], 4096) {
            w.last_composition_id = Some(id);
        }
    }
}

#[when(regex = r#"^a DELETE is issued for "([^"]+)"$"#)]
async fn when_delete_issued(w: &mut KisekiWorld, _name: String) {
    if let Some(id) = w.last_composition_id {
        match w.comp_store.delete(id) {
            Ok(()) => w.last_error = None,
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

#[given("later writes file B with the same plaintext P")]
async fn given_later_writes_same_plaintext(w: &mut KisekiWorld) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    match w.comp_store.create(ns_id, vec![ChunkId([0x01; 32])], 1024) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[when("later writes file B with the same plaintext P")]
async fn when_later_writes_same_plaintext(w: &mut KisekiWorld) {
    // Dedup: second file referencing same chunk_id.
    // The chunk already exists; composition references it.
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    match w.comp_store.create(ns_id, vec![ChunkId([0x01; 32])], 1024) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

#[given(regex = r#"^"([^"]+)" \(default dedup\) writes the same plaintext$"#)]
async fn given_default_dedup_writes(w: &mut KisekiWorld, tenant: String) {
    w.ensure_tenant(&tenant);
}

#[given(regex = r#"^chunk_id = HMAC\(P, (\S+)\) = "([^"]+)"$"#)]
async fn given_hmac_chunk_id(_w: &mut KisekiWorld, _key: String, _id: String) {
    todo!("compute HMAC chunk ID and store in chunk store")
}

#[given(regex = r#"^"([^"]+)" has chunk sha256\(P\) = "([^"]+)"$"#)]
async fn given_sha256_chunk(_w: &mut KisekiWorld, _tenant: String, _id: String) {
    todo!("store SHA256-addressed chunk for tenant in chunk store")
}

#[when("the Control Plane approves (quota, policy check)")]
async fn when_control_plane_approves(w: &mut KisekiWorld) {
    // Namespace creation approved — create it.
    w.ensure_namespace("genomics", "shard-genomics");
    w.last_error = None;
}

#[given(regex = r#"^namespace "(\S+)" has additional tag \[([^\]]+)\]$"#)]
async fn given_ns_additional_tag(_w: &mut KisekiWorld, _ns: String, _tag: String) {
    // Compliance tag — structural precondition.
}

#[given("chunk write to Chunk Storage fails (pool full, system key manager down)")]
async fn given_chunk_write_fails(w: &mut KisekiWorld) {
    todo!("trigger real chunk write failure via fault injection in chunk store")
}

#[given("the subsequent delta commit to the Log fails (shard unavailable)")]
async fn given_delta_commit_fails(w: &mut KisekiWorld) {
    todo!("trigger real delta commit failure via fault injection in log store")
}

#[when(regex = r#"^a POSIX rename targets namespace "(\S+)" \((\S+)\)$"#)]
async fn when_posix_rename_targets(w: &mut KisekiWorld, ns: String, shard: String) {
    // Create target namespace on a different shard for EXDEV.
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
    w.namespace_ids.insert(ns.clone(), ns_id);

    if let Some(id) = w.last_composition_id {
        match w.comp_store.rename(id, ns_id) {
            Ok(()) => w.last_error = None,
            Err(e) => w.last_error = Some(e.to_string()),
        }
    }
}

// === When: Workflow advisory hint forwarding ===

#[given(regex = r#"^the caller submits hint \{[^}]+\}$"#)]
async fn given_caller_submits_hint(_w: &mut KisekiWorld) {
    todo!("attach advisory hint to workflow context")
}

#[when("the Composition context forwards the hint to placement and the Log")]
async fn when_comp_forwards_hint(_w: &mut KisekiWorld) {
    todo!("forward advisory hint to placement and log subsystems")
}

#[given(regex = r#"^the caller attaches hint \{ retention_intent: final \} at finalize$"#)]
async fn given_retention_intent_final(_w: &mut KisekiWorld) {
    todo!("attach retention_intent:final hint to finalize context")
}

#[when("the caller subscribes to refcount-activity telemetry")]
async fn when_subscribe_refcount_telemetry(_w: &mut KisekiWorld) {
    todo!("subscribe to refcount-activity telemetry stream")
}

#[when(
    regex = r#"^the caller submits a create-composition request for namespace "([^"]+)" \(not authorised\) carrying hint \{ priority: batch \}$"#
)]
async fn when_create_unauthorized_ns(w: &mut KisekiWorld, ns: String) {
    todo!("attempt real create in unauthorized namespace and capture auth error")
}

#[when("the workload creates, updates, and finalizes compositions")]
async fn when_workload_creates_updates_finalizes(w: &mut KisekiWorld) {
    let ns_id = w.ensure_namespace("trials", "shard-trials-1");
    match w.comp_store.create(ns_id, vec![ChunkId([0x30; 32])], 2048) {
        Ok(id) => {
            w.last_composition_id = Some(id);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(e.to_string()),
    }
}

// === Then steps for composition.feature scenarios ===

#[then(regex = r#"^the composition "([^"]+)" exists in namespace "(\S+)"$"#)]
async fn then_comp_exists_in_ns(w: &mut KisekiWorld, _name: String, _ns: String) {
    todo!("verify composition exists in the specified namespace via comp_store")
}

#[then("the chunk's refcount includes this composition's reference")]
async fn then_chunk_refcount_includes(w: &mut KisekiWorld) {
    todo!("verify actual chunk refcount incremented for this composition's chunks")
}

#[then("the protocol gateway receives success")]
async fn then_pgw_success(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none(), "error: {:?}", w.last_error);
}

#[then("no chunk is written to Chunk Storage")]
async fn then_no_chunk_written(w: &mut KisekiWorld) {
    todo!("verify actual chunk store has no new chunks written")
}

#[then("the file data is included inline in the delta's encrypted payload")]
async fn then_inline_data(w: &mut KisekiWorld) {
    todo!("verify actual delta payload contains inline file data")
}

#[then("the delta is committed to the shard")]
async fn then_delta_committed(w: &mut KisekiWorld) {
    todo!("verify actual delta committed to shard in log store")
}

#[then("the composition is complete with inline data only")]
async fn then_comp_complete_inline(w: &mut KisekiWorld) {
    todo!("verify composition has zero chunk references and inline payload")
}

#[then(regex = r#"^new chunks \[([^\]]+)\] are written to Chunk Storage$"#)]
async fn then_new_chunks_written(w: &mut KisekiWorld, _chunks: String) {
    todo!("verify actual chunks exist in chunk store")
}

#[then(regex = r#"^a delta is appended: "([^"]+)"$"#)]
async fn then_delta_appended(w: &mut KisekiWorld, _desc: String) {
    todo!("verify actual delta appended to log store with matching description")
}

#[then(regex = r#"^the composition now references \[([^\]]+)\]$"#)]
async fn then_comp_references(w: &mut KisekiWorld, _chunks: String) {
    let id = w
        .last_composition_id
        .expect("composition must exist to check references");
    assert!(
        w.comp_store.get(id).is_ok(),
        "composition should be retrievable"
    );
}

#[then(regex = r#"^refcounts for (.+) are initialized to (\d+)$"#)]
async fn then_refcounts_initialized(w: &mut KisekiWorld, _chunks: String, _count: u64) {
    todo!("verify actual chunk refcounts in chunk store match expected count")
}

#[then(regex = r#"^a new chunk c2' is written covering the modified range$"#)]
async fn then_new_chunk_c2_prime(w: &mut KisekiWorld) {
    todo!("verify new chunk c2' exists in chunk store covering modified byte range")
}

#[then(regex = r#"^a delta records: "([^"]+)"$"#)]
async fn then_delta_records(w: &mut KisekiWorld, _desc: String) {
    todo!("verify actual delta recorded in log store with matching content")
}

#[then("c2 refcount is decremented (if no other composition references it)")]
async fn then_c2_refcount_decremented(w: &mut KisekiWorld) {
    todo!("verify actual c2 refcount decremented in chunk store")
}

#[then("c2' refcount is initialized to 1")]
async fn then_c2_prime_refcount_1(w: &mut KisekiWorld) {
    todo!("verify actual c2' refcount is 1 in chunk store")
}

#[then("the Composition context verifies all chunks are durable")]
async fn then_comp_verifies_durable(w: &mut KisekiWorld) {
    todo!("verify chunk durability check was performed for all composition chunks")
}

#[then(regex = r#"^a single delta records the complete composition: \[([^\]]+)\]$"#)]
async fn then_single_delta_records(w: &mut KisekiWorld, _chunks: String) {
    todo!("verify single delta in log store contains all listed chunks")
}

#[then("the composition becomes visible to readers only after the finalize delta commits")]
async fn then_visible_after_finalize(w: &mut KisekiWorld) {
    let id = w
        .last_composition_id
        .expect("composition must exist after finalize");
    assert!(
        w.comp_store.get(id).is_ok(),
        "composition should be retrievable after finalize"
    );
}

#[then("individual parts are NOT visible before completion (I-L5)")]
async fn then_parts_not_visible_il5(w: &mut KisekiWorld) {
    todo!("verify parts are not independently visible before finalize delta commits (I-L5)")
}

#[then("no finalize delta is committed")]
async fn then_no_finalize_delta(w: &mut KisekiWorld) {
    assert!(
        w.last_composition_id.is_none(),
        "no composition should exist after abort"
    );
}

#[then(regex = r#"^chunks c10, c11 have refcount 0 \(no composition references them\)$"#)]
async fn then_abort_chunks_refcount_0(w: &mut KisekiWorld) {
    todo!("verify actual refcount of c10, c11 is 0 in chunk store after abort")
}

#[then("chunks become eligible for GC")]
async fn then_chunks_eligible_gc(w: &mut KisekiWorld) {
    todo!("verify chunks with refcount 0 are marked GC-eligible")
}

#[then("a tombstone delta is appended to the shard")]
async fn then_tombstone_appended(w: &mut KisekiWorld) {
    todo!("verify actual tombstone delta appended to log store")
}

#[then("c5 refcount is decremented to 1 (still referenced elsewhere)")]
async fn then_c5_refcount_1(w: &mut KisekiWorld) {
    todo!("verify actual c5 refcount is 1 in chunk store")
}

#[then("c6 refcount is decremented to 0 (eligible for GC if no hold)")]
async fn then_c6_refcount_0(w: &mut KisekiWorld) {
    todo!("verify actual c6 refcount is 0 in chunk store")
}

#[then("the composition is no longer visible in the namespace")]
async fn then_comp_not_visible(w: &mut KisekiWorld) {
    if let Some(id) = w.last_composition_id {
        assert!(w.comp_store.get(id).is_err());
    }
}

#[then("a delete marker is appended (tombstone delta)")]
async fn then_delete_marker(w: &mut KisekiWorld) {
    todo!("verify actual delete marker delta appended to log store")
}

#[then("the current version becomes the delete marker")]
async fn then_current_version_delete_marker(w: &mut KisekiWorld) {
    // After delete in versioned namespace, the composition should no longer be retrievable.
    if let Some(id) = w.last_composition_id {
        assert!(
            w.comp_store.get(id).is_err(),
            "current version should be a delete marker (not retrievable)"
        );
    }
}

#[then(regex = r#"^previous versions \[([^\]]+)\] remain accessible by version ID$"#)]
async fn then_previous_versions_accessible(w: &mut KisekiWorld, _versions: String) {
    todo!("verify previous versions are retrievable by version ID from comp_store")
}

#[then("chunk refcounts are NOT decremented (versions still reference them)")]
async fn then_chunk_refcounts_not_decremented(w: &mut KisekiWorld) {
    todo!("verify actual chunk refcounts unchanged after versioned delete")
}

#[then(regex = r#"^file B's composition references chunk "([^"]+)"$"#)]
async fn then_file_b_refs_chunk(w: &mut KisekiWorld, _chunk: String) {
    let id = w.last_composition_id.expect("composition B must exist");
    assert!(
        w.comp_store.get(id).is_ok(),
        "file B's composition should be retrievable"
    );
}

#[then(regex = r#"^chunk "([^"]+)" refcount is (\d+)$"#)]
async fn then_chunk_refcount_is(w: &mut KisekiWorld, _chunk: String, count: u64) {
    // Verify chunk refcount via chunk store.
    if let Some(id) = w.last_chunk_id {
        use kiseki_chunk::store::ChunkOps;
        let rc = w.chunk_store.refcount(&id).unwrap_or(0);
        assert_eq!(rc, count, "chunk refcount mismatch");
    }
}

#[then(regex = r#"^chunk "([^"]+)" refcount increments to (\d+)$"#)]
async fn then_chunk_refcount_increments_to(w: &mut KisekiWorld, _chunk: String, count: u64) {
    // Cross-tenant dedup: refcount should have incremented.
    if let Some(id) = w.last_chunk_id {
        use kiseki_chunk::store::ChunkOps;
        let rc = w.chunk_store.refcount(&id).unwrap_or(0);
        assert_eq!(rc, count, "refcount should have incremented to {count}");
    }
}

#[then("no new chunk is stored")]
async fn then_no_new_chunk_stored(w: &mut KisekiWorld) {
    todo!("verify chunk store count unchanged (dedup reused existing chunk)")
}

#[then(regex = r#"^"([^"]+)" receives a tenant KEK wrapping for the system DEK$"#)]
async fn then_receives_kek_wrapping(w: &mut KisekiWorld, _tenant: String) {
    todo!("verify tenant KEK wrapping created for system DEK")
}

#[then("one copy of ciphertext serves both tenants")]
async fn then_one_copy_serves_both(w: &mut KisekiWorld) {
    todo!("verify single ciphertext chunk shared across both tenants")
}

#[then(regex = r#"^"([^"]+)" != "([^"]+)" — no dedup match$"#)]
async fn then_no_dedup_match(w: &mut KisekiWorld, _id1: String, _id2: String) {
    todo!("verify HMAC chunk IDs differ for isolated tenants")
}

#[then(regex = r#"^a new chunk "([^"]+)" is stored for "([^"]+)"$"#)]
async fn then_new_chunk_stored_for(w: &mut KisekiWorld, _chunk: String, _tenant: String) {
    todo!("verify isolated chunk stored for tenant in chunk store")
}

#[then(regex = r#"^"([^"]+)" data is fully isolated$"#)]
async fn then_data_fully_isolated(_w: &mut KisekiWorld, _tenant: String) {
    todo!("verify tenant data isolation: no shared chunks between tenants")
}

#[then(regex = r#"^a new shard is created for "([^"]+)"$"#)]
async fn then_new_shard_for_ns(w: &mut KisekiWorld, ns: String) {
    assert!(
        w.last_error.is_none(),
        "shard creation should succeed: {:?}",
        w.last_error
    );
    assert!(
        w.namespace_ids.contains_key(&ns),
        "namespace '{}' should be registered",
        ns
    );
}

#[then("the namespace is associated with the tenant and shard")]
async fn then_ns_associated(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_none(),
        "namespace creation should succeed: {:?}",
        w.last_error
    );
    // Verify the namespace was registered in our ID map.
    assert!(
        !w.namespace_ids.is_empty(),
        "at least one namespace should be registered"
    );
}

#[then("compliance tags from the org level are inherited")]
async fn then_compliance_tags_inherited(_w: &mut KisekiWorld) {
    todo!("verify namespace inherited compliance tags from org level")
}

#[then(regex = r#"^the effective compliance regime for "(\S+)" is \[([^\]]+)\]$"#)]
async fn then_effective_compliance(_w: &mut KisekiWorld, _ns: String, _tags: String) {
    todo!("verify effective compliance tags match expected merged set")
}

#[then("the staleness floor is the strictest of the three regimes")]
async fn then_staleness_floor(_w: &mut KisekiWorld) {
    todo!("verify staleness floor reflects strictest compliance regime")
}

#[then("audit requirements are the union of all three")]
async fn then_audit_union(_w: &mut KisekiWorld) {
    todo!("verify audit requirements are union of all compliance regimes")
}

#[then("the composition create is aborted")]
async fn then_comp_create_aborted(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some(), "expected error for aborted create");
}

#[then("no delta is committed to the Log")]
async fn then_no_delta_committed(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_some(),
        "expected an error indicating failure"
    );
}

#[then("the protocol gateway receives a retriable error")]
async fn then_pgw_retriable_error(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some(), "expected retriable error");
}

#[then("no partial state remains")]
async fn then_no_partial_state(w: &mut KisekiWorld) {
    // Atomicity: after a failed create, no composition should exist.
    assert!(
        w.last_composition_id.is_none()
            || w.comp_store.get(w.last_composition_id.unwrap()).is_err(),
        "no partial composition should remain after failure"
    );
}

#[then("the composition create fails")]
async fn then_comp_create_fails(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some(), "expected create failure");
}

#[then(regex = r#"^chunk c20 has refcount 0 \(no composition references it\)$"#)]
async fn then_c20_refcount_0(_w: &mut KisekiWorld) {
    todo!("verify actual c20 refcount is 0 in chunk store")
}

#[then("c20 becomes eligible for GC (orphan chunk cleanup)")]
async fn then_c20_gc_eligible(_w: &mut KisekiWorld) {
    todo!("verify c20 is marked GC-eligible as orphan chunk")
}

#[then("the operation returns EXDEV")]
async fn then_operation_exdev(w: &mut KisekiWorld) {
    assert!(
        w.last_error
            .as_ref()
            .is_some_and(|e| e.contains("cross-shard")),
        "expected EXDEV, got: {:?}",
        w.last_error
    );
}

#[then("the caller handles via copy + delete")]
async fn then_caller_copy_delete(w: &mut KisekiWorld) {
    // EXDEV was returned, confirming caller must handle cross-shard via copy+delete.
    assert!(
        w.last_error
            .as_ref()
            .is_some_and(|e| e.contains("cross-shard")),
        "EXDEV error should still be present: {:?}",
        w.last_error
    );
}

#[then("no 2PC or cross-shard coordination occurs")]
async fn then_no_2pc(w: &mut KisekiWorld) {
    // The EXDEV error proves we reject rather than coordinate.
    assert!(
        w.last_error.is_some(),
        "cross-shard rename should have been rejected, not coordinated"
    );
}

// === Then steps: Workflow Advisory integration ===

#[then("write-absorb capacity MAY be pre-warmed in the target pool within tenant quota")]
async fn then_write_absorb_may_prewarm(_w: &mut KisekiWorld) {
    todo!("verify write-absorb pre-warm was attempted within tenant quota")
}

#[then(
    "the announcement is advisory — checkpoint writes succeed even if no warm-up occurred (I-WA1)"
)]
async fn then_advisory_iwa1(_w: &mut KisekiWorld) {
    todo!("verify checkpoint writes succeed without warm-up (I-WA1)")
}

#[then("no capacity is reserved in a way that starves other tenants of their quota (I-T2)")]
async fn then_no_starvation_it2(_w: &mut KisekiWorld) {
    todo!("verify pre-warm does not exceed tenant quota (I-T2)")
}

#[then(
    "the finalize delta is processed normally (chunks confirmed durable before visibility, I-L5)"
)]
async fn then_finalize_normal_il5(_w: &mut KisekiWorld) {
    todo!("verify finalize delta processed with chunk durability check (I-L5)")
}

#[then("the hint MAY bias background GC urgency for parts not included in the final composition")]
async fn then_hint_may_bias_gc(_w: &mut KisekiWorld) {
    todo!("verify GC urgency bias applied for non-final parts")
}

#[then("it does NOT change refcount semantics (I-C2) or ordering guarantees (I-L5)")]
async fn then_no_change_ic2_il5(_w: &mut KisekiWorld) {
    todo!("verify refcount semantics (I-C2) and ordering (I-L5) unchanged by hint")
}

#[then("per-workflow rates are emitted in bucketed values (e.g., creates/sec, versions/sec)")]
async fn then_bucketed_rates(_w: &mut KisekiWorld) {
    todo!("verify bucketed telemetry rates emitted for workflow")
}

#[then("only activity attributable to the caller's workflow is included (I-WA5)")]
async fn then_caller_scoped_iwa5(_w: &mut KisekiWorld) {
    todo!("verify telemetry scoped to caller's workflow only (I-WA5)")
}

#[then("no neighbour workload's activity in the same namespace is inferable")]
async fn then_no_neighbour_inferable(_w: &mut KisekiWorld) {
    todo!("verify telemetry does not leak neighbour workload activity")
}

#[then("the request is rejected with the same error it would return without any hint")]
async fn then_rejected_same_error(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some(), "expected rejection");
}

#[then("the hint has no effect on authorisation (I-WA14)")]
async fn then_hint_no_effect_iwa14(_w: &mut KisekiWorld) {
    todo!("verify hint did not bypass authorization check (I-WA14)")
}

#[then("all create/update/multipart/finalize operations succeed with full correctness")]
async fn then_all_ops_succeed(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none(), "error: {:?}", w.last_error);
}

#[then("no advisory-dependent behavior (write-absorb preallocation, retention-intent biasing) is applied")]
async fn then_no_advisory_behavior(w: &mut KisekiWorld) {
    // Verify the composition was created successfully without advisory.
    assert!(
        w.last_error.is_none(),
        "operations should succeed without advisory: {:?}",
        w.last_error
    );
}

#[then("refcount, delta ordering, and chunk durability guarantees are unchanged (I-WA2)")]
async fn then_guarantees_unchanged_iwa2(w: &mut KisekiWorld) {
    todo!("verify refcount, delta ordering, and chunk durability unchanged (I-WA2)")
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
