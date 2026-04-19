//! Step definitions for view-materialization.feature.

use cucumber::{given, then, when};
use kiseki_common::ids::*;
use kiseki_view::descriptor::*;
use kiseki_view::view::{ViewOps, ViewState};

use crate::KisekiWorld;

#[given(regex = r#"^a view "(\S+)" materializing shard "(\S+)" for "(\S+)"$"#)]
async fn given_view(world: &mut KisekiWorld, view_name: String, _shard: String, _tenant: String) {
    let view_id = ViewId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        view_name.as_bytes(),
    ));
    let desc = ViewDescriptor {
        view_id,
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    };
    world.view_store.create_view(desc).unwrap();
}
