//! Step definitions for composition.feature.

use cucumber::{given, then, when};
use kiseki_common::ids::*;
use kiseki_composition::composition::CompositionOps;
use kiseki_composition::namespace::Namespace;

use crate::KisekiWorld;

#[given(regex = r#"^a namespace "(\S+)" in shard "(\S+)" owned by "(\S+)"$"#)]
async fn given_namespace(
    world: &mut KisekiWorld,
    ns_name: String,
    shard_name: String,
    tenant: String,
) {
    let shard_id = world.ensure_shard(&shard_name);
    let tenant_id = world.ensure_tenant(&tenant);
    let ns_id = NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        ns_name.as_bytes(),
    ));
    world.comp_store.add_namespace(Namespace {
        id: ns_id,
        tenant_id,
        shard_id,
        read_only: false,
    });
}

#[when(regex = r#"^a composition is created in namespace "(\S+)"$"#)]
async fn when_create_composition(world: &mut KisekiWorld, ns_name: String) {
    let ns_id = NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        ns_name.as_bytes(),
    ));
    match world.comp_store.create(ns_id, vec![], 0) {
        Ok(_id) => world.last_error = None,
        Err(e) => world.last_error = Some(e.to_string()),
    }
}

#[then("the composition is created successfully")]
async fn then_created(world: &mut KisekiWorld) {
    assert!(world.last_error.is_none(), "error: {:?}", world.last_error);
}
