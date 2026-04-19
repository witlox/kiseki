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
            .map_or(false, |e| e.contains("cross-shard")),
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
            .map_or(false, |e| e.contains("read-only")),
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
