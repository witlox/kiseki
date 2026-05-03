//! ADR-040: persistent `ViewStore` must survive process restart.
//!
//! Pins the contract that the symmetric ADR-040 §D11 line ("`ViewStore`:
//! all of it persists") imposes: a view created + advanced + reopened
//! must come back with the same descriptor, state, and watermark.
//!
//! Without `kiseki-view::persistent::PersistentRedbStorage` this test
//! fails to compile (the module doesn't exist) — that's the red
//! signal driving the implementation.

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId, ViewId};
use kiseki_view::persistent::PersistentRedbStorage;
use kiseki_view::view::{ViewOps, ViewStore};
use kiseki_view::{ConsistencyModel, ProtocolSemantics, ViewDescriptor, ViewState};

#[test]
fn views_and_watermarks_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("views.redb");

    let view_id = ViewId(uuid::Uuid::from_u128(0xC0DE));
    let descriptor = ViewDescriptor {
        view_id,
        tenant_id: OrgId(uuid::Uuid::from_u128(1)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(2))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    };

    // Open the persistent store, create a view, advance its watermark
    // to a non-trivial value, then drop everything (= simulate process
    // exit).
    {
        let storage = PersistentRedbStorage::open(&path).expect("open persistent view storage");
        let mut store = ViewStore::with_storage(Box::new(storage));
        store.create_view(descriptor.clone()).expect("create_view");
        store
            .advance_watermark(view_id, SequenceNumber(42), 1_700_000_000_000)
            .expect("advance_watermark");
        // Confirm in-process state is what we expect before the drop.
        let view = store.get_view(view_id).expect("get_view pre-drop");
        assert_eq!(view.watermark, SequenceNumber(42));
        assert_eq!(view.state, ViewState::Active);
    }

    // Re-open from the same path. The view + state + watermark must
    // be present without any replay.
    let storage = PersistentRedbStorage::open(&path).expect("re-open persistent view storage");
    let store = ViewStore::with_storage(Box::new(storage));

    let view = store
        .get_view(view_id)
        .expect("view should exist after reopen");
    assert_eq!(
        view.descriptor.view_id, view_id,
        "descriptor.view_id must round-trip across restart",
    );
    assert_eq!(
        view.descriptor.tenant_id, descriptor.tenant_id,
        "descriptor.tenant_id must round-trip",
    );
    assert_eq!(
        view.descriptor.source_shards, descriptor.source_shards,
        "descriptor.source_shards must round-trip",
    );
    assert_eq!(
        view.state,
        ViewState::Active,
        "state must round-trip — Active became Active after watermark > 0",
    );
    assert_eq!(
        view.watermark,
        SequenceNumber(42),
        "watermark must be exactly what we last persisted, not 0",
    );
    assert!(
        view.pins.is_empty(),
        "pins are session-scoped (TTL'd in ms); reopening drops them \
         and clients re-acquire — see ADR-040 §D11",
    );
}
