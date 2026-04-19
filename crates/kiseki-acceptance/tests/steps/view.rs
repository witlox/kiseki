//! Step definitions for view-materialization.feature.

use crate::KisekiWorld;
use cucumber::{given, then, when};
use kiseki_common::ids::*;
use kiseki_view::descriptor::*;
use kiseki_view::view::{ViewOps, ViewState};

fn test_descriptor(name: &str) -> ViewDescriptor {
    ViewDescriptor {
        view_id: ViewId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        )),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    }
}

#[when(regex = r#"^a view "(\S+)" is created from descriptor$"#)]
async fn when_create_view(w: &mut KisekiWorld, name: String) {
    let desc = test_descriptor(&name);
    let id = w.view_store.create_view(desc).unwrap();
    w.last_view_id = Some(id);
    w.view_ids.insert(name, id);
}

#[then(regex = r#"^the view state is "(\S+)"$"#)]
async fn then_view_state(w: &mut KisekiWorld, expected: String) {
    let id = w.last_view_id.unwrap();
    let view = w.view_store.get_view(id).unwrap();
    let state_str = match view.state {
        ViewState::Building => "Building",
        ViewState::Active => "Active",
        ViewState::Discarded => "Discarded",
    };
    assert_eq!(state_str, expected);
}

#[when(regex = r#"^the watermark is advanced to (\d+)$"#)]
async fn when_advance(w: &mut KisekiWorld, pos: u64) {
    let id = w.last_view_id.unwrap();
    w.view_store
        .advance_watermark(id, SequenceNumber(pos), 1000)
        .unwrap();
}

#[when(regex = r#"^the view "(\S+)" is discarded$"#)]
async fn when_discard(w: &mut KisekiWorld, name: String) {
    let id = *w.view_ids.get(&name).unwrap();
    w.view_store.discard_view(id).unwrap();
    w.last_view_id = Some(id);
}

#[when(regex = r#"^an MVCC read pin is acquired with TTL (\d+)ms$"#)]
async fn when_pin(w: &mut KisekiWorld, ttl: u64) {
    let id = w.last_view_id.unwrap();
    let pin_id = w.view_store.acquire_pin(id, ttl, 1000).unwrap();
    w.last_sequence = Some(SequenceNumber(pin_id));
}

#[then(regex = r#"^the pin holds a snapshot at the current watermark$"#)]
async fn then_pin_holds(w: &mut KisekiWorld) {
    let id = w.last_view_id.unwrap();
    let view = w.view_store.get_view(id).unwrap();
    assert!(!view.pins.is_empty());
}

#[when(regex = r#"^(\d+)ms pass$"#)]
async fn when_time_passes(_w: &mut KisekiWorld, _ms: u64) {
    // Time is simulated via explicit now_ms parameters
}

#[then("the pin expires")]
async fn then_pin_expires(w: &mut KisekiWorld) {
    let id = w.last_view_id.unwrap();
    let expired = w.view_store.expire_pins(id, 100_000); // far future
    assert!(expired > 0, "pin should have expired");
}
