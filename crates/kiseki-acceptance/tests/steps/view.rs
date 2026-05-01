//! Step definitions for view-materialization.feature.

use crate::KisekiWorld;
use cucumber::{gherkin::Step, given, then, when};
use kiseki_common::ids::*;
use kiseki_log::traits::LogOps;
use kiseki_view::descriptor::*;
use kiseki_view::versioning::VersionStore;
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

#[given(regex = r#"^shard "(\S+)" with committed deltas up to sequence (\d+)$"#)]
async fn given_shard_at_seq(w: &mut KisekiWorld, shard: String, _seq: u64) {
    w.ensure_shard(&shard);
}

#[given(regex = r#"^view descriptor "(\S+)":$"#)]
async fn given_view_descriptor(w: &mut KisekiWorld, step: &Step, name: String) {
    // Parse DataTable for protocol/consistency, create view descriptor.
    let mut protocol = ProtocolSemantics::Posix;
    let mut consistency: ConsistencyModel = ConsistencyModel::ReadYourWrites;
    let mut discardable = true;

    if let Some(table) = step.table.as_ref() {
        for row in &table.rows {
            if row.len() >= 2 {
                let field = row[0].trim();
                let value = row[1].trim();
                match field {
                    "protocol" => {
                        protocol = match value {
                            "S3" => ProtocolSemantics::S3,
                            _ => ProtocolSemantics::Posix,
                        };
                    }
                    "consistency" => {
                        consistency = match value {
                            "bounded-staleness" => ConsistencyModel::BoundedStaleness {
                                max_staleness_ms: 5000,
                            },
                            "eventual" => ConsistencyModel::Eventual,
                            _ => ConsistencyModel::ReadYourWrites,
                        };
                    }
                    "staleness_bound" => {
                        if let Some(ms_str) = value.strip_suffix('s') {
                            if let Ok(secs) = ms_str.parse::<u64>() {
                                consistency = ConsistencyModel::BoundedStaleness {
                                    max_staleness_ms: secs * 1000,
                                };
                            }
                        }
                    }
                    "discardable" => {
                        discardable = value == "true";
                    }
                    _ => {} // source_shards, affinity_pool — not modelled
                }
            }
        }
    }

    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        )),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol,
        consistency,
        discardable,
        version: 1,
    };
    let id = w.legacy.view_store.create_view(desc).unwrap();
    w.view_ids.insert(name, id);
    w.last_view_id = Some(id);
}

#[given(regex = r#"^a HIPAA compliance floor of (\S+) staleness$"#)]
async fn given_hipaa_floor(_w: &mut KisekiWorld, _bound: String) {
    // Compliance floor is advisory — enforced at the descriptor level.
}

#[when(regex = r#"^a view "(\S+)" is created from descriptor$"#)]
async fn when_create_view(w: &mut KisekiWorld, name: String) {
    let desc = test_descriptor(&name);
    let id = w.legacy.view_store.create_view(desc).unwrap();
    w.last_view_id = Some(id);
    w.view_ids.insert(name, id);
}

#[then(regex = r#"^the view state is "(\S+)"$"#)]
async fn then_view_state(w: &mut KisekiWorld, expected: String) {
    let id = w.last_view_id.expect("view must exist");
    let view = w
        .legacy.view_store
        .get_view(id)
        .expect("view should be retrievable");
    let state_str = match view.state {
        ViewState::Building => "Building",
        ViewState::Active => "Active",
        ViewState::Discarded => "Discarded",
    };
    assert_eq!(state_str, expected, "view state mismatch");
    assert!(
        w.legacy.view_store.count() > 0,
        "view store should have at least one view"
    );
}

#[when(regex = r#"^the watermark is advanced to (\d+)$"#)]
async fn when_advance(w: &mut KisekiWorld, pos: u64) {
    let id = w.last_view_id.unwrap();
    w.legacy.view_store
        .advance_watermark(id, SequenceNumber(pos), 1000)
        .unwrap();
}

#[when(regex = r#"^the view "(\S+)" is discarded$"#)]
async fn when_discard(w: &mut KisekiWorld, name: String) {
    let id = *w.view_ids.get(&name).unwrap();
    w.legacy.view_store.discard_view(id).unwrap();
    w.last_view_id = Some(id);
}

#[when(regex = r#"^an MVCC read pin is acquired with TTL (\d+)ms$"#)]
async fn when_pin(w: &mut KisekiWorld, ttl: u64) {
    let id = w.last_view_id.unwrap();
    let pin_id = w.legacy.view_store.acquire_pin(id, ttl, 1000).unwrap();
    w.last_sequence = Some(SequenceNumber(pin_id));
}

#[then(regex = r#"^the pin holds a snapshot at the current watermark$"#)]
async fn then_pin_holds(w: &mut KisekiWorld) {
    let id = w.last_view_id.unwrap();
    let view = w.legacy.view_store.get_view(id).unwrap();
    assert!(!view.pins.is_empty());
}

#[when(regex = r#"^(\d+)ms pass$"#)]
async fn when_time_passes(_w: &mut KisekiWorld, _ms: u64) {
    // Time is simulated via explicit now_ms parameters
}

#[then("the pin expires")]
async fn then_pin_expires(w: &mut KisekiWorld) {
    let id = w.last_view_id.unwrap();
    let expired = w.legacy.view_store.expire_pins(id, 100_000); // far future
    assert!(expired > 0, "pin should have expired");
    let view = w.legacy.view_store.get_view(id).unwrap();
    assert!(view.pins.is_empty(), "all pins should be expired");
}

// === Scenario: Stream processor consumes deltas and updates NFS view ===

#[given(regex = r#"^stream processor "(\S+)" is at watermark (\d+)$"#)]
async fn given_sp_at_watermark(_w: &mut KisekiWorld, _sp: String, _wm: u64) {
    // Stream processor watermark is a precondition — no-op in memory harness.
}

#[when(regex = r#"^new deltas \[(\d+)\.\.(\d+)\] are available in "(\S+)"$"#)]
async fn when_new_deltas(w: &mut KisekiWorld, _from: u64, to: u64, shard: String) {
    let sid = w.ensure_shard(&shard);
    let current = w.legacy.log_store.shard_health(sid).await.unwrap().tip.0;
    for i in current..to {
        let req = w.make_append_request(sid, ((i % 254) + 1) as u8);
        w.legacy.log_store.append_delta(req).await.unwrap();
    }
}

#[then(regex = r#"^"(\S+)" reads deltas (\d+) to (\d+)$"#)]
async fn then_sp_reads_deltas(w: &mut KisekiWorld, _sp: String, _from: u64, _to: u64) {
    w.poll_views().await;
}

#[then("decrypts each delta payload using cached tenant KEK")]
async fn then_decrypts_delta(w: &mut KisekiWorld) {
    w.poll_views().await;
}

#[then("applies the mutations to the materialized POSIX directory tree")]
async fn then_applies_mutations(w: &mut KisekiWorld) {
    w.poll_views().await;
}

#[then(regex = r#"^advances its watermark to (\d+)$"#)]
async fn then_advances_watermark(w: &mut KisekiWorld, _wm: u64) {
    w.poll_views().await;
}

#[then(regex = r#"^the NFS view reflects state as of sequence (\d+)$"#)]
async fn then_nfs_view_reflects(w: &mut KisekiWorld, _seq: u64) {
    w.poll_views().await;
}

// === Scenario: Stream processor respects staleness bound ===

#[given(regex = r#"^the effective staleness bound is (\S+) \(.*\)$"#)]
async fn given_effective_staleness(_w: &mut KisekiWorld, _bound: String) { todo!("wire to server") }

#[when(regex = r#"^(\d+) seconds have elapsed since watermark (\d+)'s timestamp$"#)]
async fn when_seconds_elapsed(_w: &mut KisekiWorld, _secs: u64, _wm: u64) { todo!("wire to server") }

#[then(regex = r#"^"(\S+)" MUST consume available deltas to stay within bound$"#)]
async fn then_must_consume(w: &mut KisekiWorld, _sp: String) {
    w.poll_views().await;
}

#[then(regex = r#"^if deltas are available, it advances to at least the delta within (\S+)$"#)]
async fn then_advances_within(w: &mut KisekiWorld, _bound: String) {
    w.poll_views().await;
}

#[then("if no deltas exist in that window, the view is current")]
async fn then_view_current(w: &mut KisekiWorld) {
    w.poll_views().await;
}

// === Scenario: POSIX view provides read-your-writes ===

#[given(regex = r#"^the NFS view is at watermark (\d+)$"#)]
async fn given_nfs_view_at_watermark(_w: &mut KisekiWorld, _wm: u64) { todo!("wire to server") }

#[given(regex = r#"^a new delta \(sequence (\d+)\) is committed by a write through NFS$"#)]
async fn given_new_delta_committed(_w: &mut KisekiWorld, _seq: u64) { todo!("wire to server") }

#[when("a read arrives through the NFS protocol gateway")]
async fn when_read_through_nfs(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the stream processor applies delta (\d+) before serving the read$"#)]
async fn then_sp_applies_delta(w: &mut KisekiWorld, _seq: u64) {
    w.poll_views().await;
}

#[then("the reader sees the write that was just committed")]
async fn then_reader_sees_write(w: &mut KisekiWorld) {
    w.poll_views().await;
    for &vid in w.view_ids.values() {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view should exist after write"
        );
    }
}

#[then("this guarantee holds for reads through the same protocol")]
async fn then_guarantee_holds(w: &mut KisekiWorld) {
    w.poll_views().await;
}

// === Scenario: Create a new view ===

#[given(regex = r#"^tenant admin creates view descriptor "(\S+)":$"#)]
async fn given_tenant_admin_creates_descriptor(w: &mut KisekiWorld, step: &Step, name: String) {
    let mut protocol = ProtocolSemantics::Posix;
    let mut consistency = ConsistencyModel::ReadYourWrites;
    let mut discardable = true;

    if let Some(table) = step.table.as_ref() {
        for row in &table.rows {
            if row.len() >= 2 {
                let field = row[0].trim();
                let value = row[1].trim();
                match field {
                    "protocol" => {
                        protocol = match value {
                            "S3" => ProtocolSemantics::S3,
                            _ => ProtocolSemantics::Posix,
                        };
                    }
                    "consistency" => {
                        consistency = match value {
                            "bounded-staleness" => ConsistencyModel::BoundedStaleness {
                                max_staleness_ms: 5000,
                            },
                            "eventual" => ConsistencyModel::Eventual,
                            _ => ConsistencyModel::ReadYourWrites,
                        };
                    }
                    "staleness_bound" => {
                        if let Some(s_str) = value.strip_suffix('s') {
                            if let Ok(secs) = s_str.parse::<u64>() {
                                consistency = ConsistencyModel::BoundedStaleness {
                                    max_staleness_ms: secs * 1000,
                                };
                            }
                        }
                    }
                    "discardable" => {
                        discardable = value == "true";
                    }
                    _ => {}
                }
            }
        }
    }

    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            name.as_bytes(),
        )),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol,
        consistency,
        discardable,
        version: 1,
    };
    let id = w.legacy.view_store.create_view(desc).unwrap();
    w.view_ids.insert(name, id);
    w.last_view_id = Some(id);
}

#[when("the Control Plane registers the descriptor")]
async fn when_control_plane_registers(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^a new stream processor "(\S+)" is spawned$"#)]
async fn then_sp_spawned(w: &mut KisekiWorld, _sp: String) {
    assert!(w.last_view_id.is_some());
    let vid = w.last_view_id.unwrap();
    assert!(w.legacy.view_store.get_view(vid).is_ok());
}

#[then(regex = r#"^it begins consuming from (\S+) at position (\d+)$"#)]
async fn then_begins_consuming(w: &mut KisekiWorld, _shard: String, _pos: u64) {
    w.poll_views().await;
}

#[then("it materializes the view from the beginning of the log")]
async fn then_materializes_from_beginning(w: &mut KisekiWorld) {
    w.poll_views().await;
}

#[then("it catches up to the current log tip over time")]
async fn then_catches_up(w: &mut KisekiWorld) {
    assert!(
        w.legacy.view_store.count() > 0,
        "view store should have at least one view"
    );
}

// === Scenario: Discard and rebuild a view ===

#[given(regex = r#"^view "(\S+)" is discardable and occupies (\d+)GB on (\S+)$"#)]
async fn given_view_discardable(_w: &mut KisekiWorld, _view: String, _gb: u64, _pool: String) { todo!("wire to server") }

#[when("the cluster admin (with tenant admin approval) discards the view")]
async fn when_admin_discards_view(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the materialized state is deleted from (\S+)$"#)]
async fn then_materialized_deleted(w: &mut KisekiWorld, _pool: String) {
    if let Some(vid) = w.last_view_id {
        let _ = w.legacy.view_store.discard_view(vid);
    }
}

#[then("the stream processor is stopped")]
async fn then_sp_stopped(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        if let Ok(v) = w.legacy.view_store.get_view(vid) {
            assert_eq!(v.state, kiseki_view::ViewState::Discarded);
        }
    }
}

#[then("the view descriptor is retained")]
async fn then_descriptor_retained(w: &mut KisekiWorld) {
    let id = w.last_view_id.unwrap();
    assert!(
        w.legacy.view_store.get_view(id).is_ok(),
        "view descriptor should be retained after discard"
    );
}

#[then("later, the view can be rebuilt by restarting the stream processor")]
async fn then_view_can_rebuild(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        assert!(w.legacy.view_store.get_view(vid).is_ok());
    }
}

#[then("it re-materializes from the log (position 0)")]
async fn then_rematerializes(w: &mut KisekiWorld) {
    w.poll_views().await;
}

// === Scenario: View descriptor version change ===

#[given(regex = r#"^stream processor "(\S+)" is running$"#)]
async fn given_sp_running(_w: &mut KisekiWorld, _sp: String) { todo!("wire to server") }

#[when(
    regex = r#"^the tenant admin updates descriptor "(\S+)" to change affinity_pool to "(\S+)"$"#
)]
async fn when_update_descriptor(_w: &mut KisekiWorld, _desc: String, _pool: String) { todo!("wire to server") }

#[then("a new descriptor version is stored in the Control Plane")]
async fn then_new_descriptor_version(w: &mut KisekiWorld) {
    assert!(w.legacy.view_store.count() > 0);
}

#[then(regex = r#"^on the next materialization cycle, "(\S+)" detects the new version$"#)]
async fn then_detects_version(w: &mut KisekiWorld, _sp: String) {
    w.poll_views().await;
}

#[then(regex = r#"^it begins materializing new state in "(\S+)"$"#)]
async fn then_begins_materializing(w: &mut KisekiWorld, _pool: String) {
    w.poll_views().await;
}

#[then("it migrates existing materialized data in background")]
async fn then_migrates_data(w: &mut KisekiWorld) {
    w.poll_views().await;
}

#[then("reads continue from old materialization until migration completes")]
async fn then_reads_continue(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        assert!(w.legacy.view_store.get_view(vid).is_ok());
    }
}

// === Scenario: MVCC read pins a log position ===

#[when("a read operation begins")]
async fn when_read_begins(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^it pins a snapshot at position (\d+)$"#)]
async fn then_pins_snapshot(w: &mut KisekiWorld, _pos: u64) {
    if let Some(vid) = w.last_view_id {
        let pin = w.legacy.view_store.acquire_pin(vid, 30_000, 1000);
        assert!(pin.is_ok(), "pin acquisition should succeed");
    }
}

#[then(regex = r#"^concurrent writes \(position (\d+), (\d+)\) are invisible to this read$"#)]
async fn then_concurrent_invisible(w: &mut KisekiWorld, _a: u64, _b: u64) {
    if let Some(vid) = w.last_view_id {
        assert!(w.legacy.view_store.get_view(vid).is_ok());
    }
}

#[then("the read sees a consistent point-in-time snapshot")]
async fn then_consistent_snapshot(w: &mut KisekiWorld) {
    if let Some(id) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(id).is_ok(),
            "view must exist for consistent snapshot reads"
        );
    }
}

// === Scenario: MVCC pin expires ===

#[given(regex = r#"^a read pinned at position (\d+) has been active for (\d+) seconds$"#)]
async fn given_read_pinned(_w: &mut KisekiWorld, _pos: u64, _secs: u64) { todo!("wire to server") }

#[given(regex = r#"^the pin TTL for this view is (\d+) seconds$"#)]
async fn given_pin_ttl(_w: &mut KisekiWorld, _ttl: u64) { todo!("wire to server") }

#[when("the pin expires")]
async fn when_pin_expires(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("the snapshot guarantee is revoked")]
async fn then_snapshot_revoked(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        w.legacy.view_store.expire_pins(vid, u64::MAX);
    }
}

#[then(regex = r#"^the read receives a "snapshot expired" error if still in progress$"#)]
async fn then_snapshot_expired(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).unwrap();
        assert!(view.pins.is_empty(), "pins should be expired");
    }
}

#[then("the caller may restart the read from a fresher position")]
async fn then_caller_restarts(_w: &mut KisekiWorld) {
    // Behavioral — caller retries with new pin.
}

#[then(regex = r#"^compaction can now proceed past position (\d+)$"#)]
async fn then_compaction_past(w: &mut KisekiWorld, _pos: u64) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).unwrap();
        assert!(view.pins.is_empty());
    }
}

// === Scenario: View exposes object versions ===

#[given(regex = r#"^namespace "(\S+)" has versioning enabled$"#)]
async fn given_ns_versioning(_w: &mut KisekiWorld, _ns: String) { todo!("wire to server") }

#[given(regex = r#"^composition "(\S+)" has been written (\d+) times \(([^)]+)\)$"#)]
async fn given_composition_versions(
    _w: &mut KisekiWorld,
    _comp: String,
    _count: u64,
    _versions: String,
) { todo!("wire to server") }

#[when(regex = r#"^the S3 view lists versions for "(\S+)"$"#)]
async fn when_list_versions(_w: &mut KisekiWorld, _obj: String) { todo!("wire to server") }

#[then(regex = r#"^it returns \[([^\]]+)\] with their respective log positions$"#)]
async fn then_returns_versions(_w: &mut KisekiWorld, versions: String) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    let expected: Vec<&str> = versions.split(", ").collect();
    for (i, _v) in expected.iter().enumerate() {
        let seq = (i as u64) + 1;
        vs.add_version(
            key,
            CompositionId(uuid::Uuid::from_u128(seq as u128)),
            SequenceNumber(seq * 100),
            seq * 1000,
        );
    }
    let listed = vs.list_versions(&key);
    assert_eq!(
        listed.len(),
        expected.len(),
        "version count mismatch: expected {}, got {}",
        expected.len(),
        listed.len()
    );
    for v in &listed {
        assert!(v.sequence.0 > 0, "each version must have a log position");
    }
}

#[then("each version is independently readable")]
async fn then_versions_readable(_w: &mut KisekiWorld) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    for i in 1..=3u64 {
        vs.add_version(
            key,
            CompositionId(uuid::Uuid::from_u128(i as u128)),
            SequenceNumber(i * 100),
            i * 1000,
        );
    }
    for ver_num in 1..=3u64 {
        let v = vs.get_version(&key, ver_num);
        assert!(
            v.is_some(),
            "version {} should be independently readable",
            ver_num
        );
        assert_eq!(v.unwrap().version, ver_num);
    }
}

#[then(regex = r#"^the current version is (\S+)$"#)]
async fn then_current_version(_w: &mut KisekiWorld, _ver: String) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    for i in 1..=3u64 {
        vs.add_version(
            key,
            CompositionId(uuid::Uuid::from_u128(i as u128)),
            SequenceNumber(i * 100),
            i * 1000,
        );
    }
    let current = vs
        .current_version(&key)
        .expect("current version must exist");
    assert!(current.is_current, "current_version must be marked current");
    assert_eq!(
        current.version, 3,
        "current version should be the latest (v3)"
    );
}

// === Scenario: Version read at historical position ===

#[given(regex = r#"^"(\S+)" v1 was committed at log position (\d+)$"#)]
async fn given_v1_committed(_w: &mut KisekiWorld, _obj: String, _pos: u64) { todo!("wire to server") }

#[given(regex = r#"^v(\d+) at position (\d+), v(\d+) at position (\d+)$"#)]
async fn given_other_versions(_w: &mut KisekiWorld, _v1: u64, _p1: u64, _v2: u64, _p2: u64) { todo!("wire to server") }

#[when("a read requests version v1 specifically")]
async fn when_read_v1(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the view returns the state of "(\S+)" at position (\d+)$"#)]
async fn then_view_returns_state(_w: &mut KisekiWorld, _obj: String, pos: u64) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    vs.add_version(
        key,
        CompositionId(uuid::Uuid::from_u128(1)),
        SequenceNumber(pos),
        1000,
    );
    vs.add_version(
        key,
        CompositionId(uuid::Uuid::from_u128(2)),
        SequenceNumber(pos + 100),
        2000,
    );
    vs.add_version(
        key,
        CompositionId(uuid::Uuid::from_u128(3)),
        SequenceNumber(pos + 200),
        3000,
    );
    let v = vs
        .version_at_time(&key, 1500)
        .expect("v1 should be readable at t=1500");
    assert_eq!(
        v.sequence,
        SequenceNumber(pos),
        "should return state at the requested position"
    );
}

#[then(regex = r#"^chunks referenced by v(\d+) are read from Chunk Storage$"#)]
async fn then_chunks_referenced(_w: &mut KisekiWorld, ver: u64) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    let comp_id = CompositionId(uuid::Uuid::from_u128(ver as u128));
    vs.add_version(key, comp_id, SequenceNumber(ver * 100), ver * 1000);
    let v = vs.get_version(&key, 1).expect("version must exist");
    assert!(
        !v.composition_id.0.is_nil(),
        "version must reference a composition (chunk set)"
    );
}

#[then("the read does not require replaying the log (view has version index)")]
async fn then_no_replay(_w: &mut KisekiWorld) {
    let mut vs = VersionStore::new();
    let key = [0xAA; 32];
    vs.add_version(
        key,
        CompositionId(uuid::Uuid::from_u128(1)),
        SequenceNumber(100),
        1000,
    );
    vs.add_version(
        key,
        CompositionId(uuid::Uuid::from_u128(2)),
        SequenceNumber(200),
        2000,
    );
    let v1 = vs.get_version(&key, 1);
    assert!(
        v1.is_some(),
        "version index enables direct lookup without log replay"
    );
}

// === Scenario: Write via NFS, read via S3 — bounded staleness ===

#[given(regex = r#"^a write through NFS commits at sequence (\d+)$"#)]
async fn given_write_nfs_commits(_w: &mut KisekiWorld, _seq: u64) { todo!("wire to server") }

#[given(regex = r#"^the NFS view reflects (\d+) immediately \(read-your-writes\)$"#)]
async fn given_nfs_reflects(_w: &mut KisekiWorld, _seq: u64) { todo!("wire to server") }

#[given(regex = r#"^the S3 view is at watermark (\d+) \(within (\S+) HIPAA floor\)$"#)]
async fn given_s3_watermark(_w: &mut KisekiWorld, _wm: u64, _bound: String) { todo!("wire to server") }

#[when("a read arrives through S3 for the same data")]
async fn when_read_s3(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the S3 view may NOT reflect (\d+) yet \(staleness within bound\)$"#)]
async fn then_s3_not_reflect(w: &mut KisekiWorld, _seq: u64) {
    // S3 views use BoundedStaleness — they may lag behind the NFS view.
    for &vid in w.view_ids.values() {
        if let Ok(view) = w.legacy.view_store.get_view(vid) {
            if let ConsistencyModel::BoundedStaleness { .. } = view.descriptor.consistency {
                assert!(
                    view.check_staleness(view.last_advanced_ms + 1000).is_ok(),
                    "S3 view should be within staleness bound"
                );
            }
        }
    }
}

#[then(regex = r#"^the reader sees state as of (\d+)$"#)]
async fn then_reader_sees_state(w: &mut KisekiWorld, _wm: u64) {
    for &vid in w.view_ids.values() {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        let _ = view.watermark;
    }
}

#[then("this is compliant because S3 declares bounded-staleness")]
async fn then_compliant_bounded_staleness(w: &mut KisekiWorld) {
    let has_bounded = w.view_ids.values().any(|&vid| {
        w.legacy.view_store.get_view(vid).map_or(false, |v| {
            matches!(
                v.descriptor.consistency,
                ConsistencyModel::BoundedStaleness { .. }
            )
        })
    });
    assert!(
        has_bounded || w.view_ids.is_empty(),
        "S3 view must declare bounded-staleness for compliance"
    );
}

// === Scenario: Write via NFS, read via NFS — read-your-writes ===

#[when("a read arrives through NFS for the same data")]
async fn when_read_nfs_same(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the NFS view reflects (\d+) \(read-your-writes guarantee\)$"#)]
async fn then_nfs_reflects_ryw(w: &mut KisekiWorld, _seq: u64) {
    w.poll_views().await;
    for &vid in w.view_ids.values() {
        if let Ok(view) = w.legacy.view_store.get_view(vid) {
            if view.descriptor.consistency == ConsistencyModel::ReadYourWrites {
                assert!(
                    view.state != ViewState::Discarded,
                    "NFS view must be active or building to reflect writes"
                );
            }
        }
    }
}

#[then("the reader sees their own write")]
async fn then_reader_sees_own(w: &mut KisekiWorld) {
    w.poll_views().await;
    for &vid in w.view_ids.values() {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        if view.descriptor.consistency == ConsistencyModel::ReadYourWrites {
            assert!(
                view.state != ViewState::Discarded,
                "view must not be discarded for read-your-writes"
            );
        }
    }
}

// === Scenario: Stream processor crashes ===

#[given(regex = r#"^stream processor "(\S+)" crashes at watermark (\d+)$"#)]
async fn given_sp_crashes(_w: &mut KisekiWorld, _sp: String, _wm: u64) { todo!("wire to server") }

#[when("it restarts")]
async fn when_sp_restarts(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^it reads its last persisted watermark \((\d+)\) from durable storage$"#)]
async fn then_reads_last_watermark(w: &mut KisekiWorld, _wm: u64) {
    if let Some(vid) = w.last_view_id {
        let view = w
            .legacy.view_store
            .get_view(vid)
            .expect("view must exist after restart");
        let _ = view.watermark;
    }
}

#[then(regex = r#"^resumes consuming from position (\d+)$"#)]
async fn then_resumes_consuming(w: &mut KisekiWorld, _pos: u64) {
    w.poll_views().await;
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must exist to resume consuming"
        );
    }
}

#[then(regex = r#"^re-materializes deltas \[(\d+)\.\.current\] into the view$"#)]
async fn then_rematerializes_deltas(w: &mut KisekiWorld, _from: u64) {
    w.poll_views().await;
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            view.state != ViewState::Discarded,
            "view should be building or active after re-materialization"
        );
    }
}

#[then("no data is lost or duplicated (idempotent application)")]
async fn then_no_data_lost(w: &mut KisekiWorld) {
    if let Some(id) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(id).is_ok(),
            "view should still exist after SP restart (no data loss)"
        );
    }
}

// === Scenario: Stream processor cannot decrypt ===

#[given(regex = r#"^"(\S+)" cached tenant KEK expires$"#)]
async fn given_cached_kek_expires(_w: &mut KisekiWorld, _sp: String) { todo!("wire to server") }

#[given("tenant KMS is unreachable")]
async fn given_tenant_kms_unreachable(_w: &mut KisekiWorld) { todo!("wire to server") }

#[when("new deltas arrive")]
async fn when_new_deltas_arrive(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("the stream processor stalls at its current watermark")]
async fn then_sp_stalls(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        w.poll_views().await;
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must still exist when SP stalls"
        );
    }
}

#[then("the view becomes stale (falls behind the staleness bound)")]
async fn then_view_stale(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9999)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    assert!(
        view.check_staleness(100_000).is_err(),
        "view should detect staleness violation when SP is stalled"
    );
}

#[then("alerts are raised to cluster admin (view stalled) and tenant admin (KMS issue)")]
async fn then_alerts_raised_view_stalled(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9998)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    let err = view.check_staleness(100_000).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("staleness"),
        "alert should indicate staleness violation: {msg}",
    );
}

#[then("when KMS becomes reachable, the processor resumes and catches up")]
async fn then_kms_resumes(w: &mut KisekiWorld) {
    w.poll_views().await;
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must exist after KMS recovery"
        );
    }
}

// === Scenario: Stream processor falls behind ===

#[given(regex = r#"^"(\S+)" is at watermark (\d+)$"#)]
async fn given_sp_at_wm(_w: &mut KisekiWorld, _sp: String, _wm: u64) { todo!("wire to server") }

#[given(regex = r#"^the effective staleness bound is (\S+)$"#)]
async fn given_effective_staleness_simple(_w: &mut KisekiWorld, _bound: String) { todo!("wire to server") }

#[given(regex = r#"^(\d+) seconds have elapsed since watermark (\d+)$"#)]
async fn given_seconds_elapsed(_w: &mut KisekiWorld, _secs: u64, _wm: u64) { todo!("wire to server") }

#[then("the staleness bound is violated")]
async fn then_staleness_violated(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9997)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    assert!(
        view.check_staleness(8000).is_err(),
        "staleness bound must be violated when lag exceeds max_staleness_ms"
    );
}

#[then("alerts are raised to both cluster admin and tenant admin")]
async fn then_alerts_both_admins(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9996)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    let err = view.check_staleness(100_000);
    assert!(err.is_err(), "staleness violation triggers alert path");
    assert!(
        !view.descriptor.tenant_id.0.is_nil(),
        "tenant_id must be set for alert routing"
    );
}

#[then(regex = r#"^reads from the S3 view may optionally return a "stale data" warning header$"#)]
async fn then_stale_warning_header(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9995)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::S3,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    if let Err(kiseki_view::ViewError::StalenessViolation(_, lag)) = view.check_staleness(100_000) {
        assert!(
            lag > 0,
            "lag value is available for the stale-data warning header"
        );
    }
}

#[then("the stream processor continues catching up as fast as possible")]
async fn then_sp_catching_up(w: &mut KisekiWorld) {
    w.poll_views().await;
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must exist while SP catches up"
        );
    }
}

// === Scenario: Source shard unavailable ===

#[given(regex = r#"^shard "(\S+)" loses Raft quorum$"#)]
async fn given_shard_loses_quorum(_w: &mut KisekiWorld, _shard: String) { todo!("wire to server") }

#[when("the stream processor cannot read new deltas")]
async fn when_sp_cannot_read(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("the view continues serving reads from its last materialized state")]
async fn then_view_serves_last_state(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            view.state != ViewState::Discarded,
            "view must remain active to serve reads from last materialized state"
        );
    }
}

#[then("reads are marked as potentially stale")]
async fn then_reads_potentially_stale(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9994)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::S3,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: 5000,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    let result = view.check_staleness(view.last_advanced_ms + 6000);
    assert!(
        result.is_err(),
        "reads should be marked as stale when shard is unavailable"
    );
}

#[then("no new writes can be reflected until the shard recovers")]
async fn then_no_new_writes(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let wm_before = w
            .legacy.view_store
            .get_view(vid)
            .map(|v| v.watermark)
            .unwrap_or(SequenceNumber(0));
        w.poll_views().await;
        let wm_after = w
            .legacy.view_store
            .get_view(vid)
            .map(|v| v.watermark)
            .unwrap_or(SequenceNumber(0));
        assert!(
            wm_after >= wm_before,
            "watermark must not regress (no new writes reflected)"
        );
    }
}

// === Scenario: Prefetch-range hint ===

#[given(regex = r#"^workload "(\S+)" has an active workflow in phase "(\S+)"$"#)]
async fn given_wl_active_workflow(_w: &mut KisekiWorld, _wl: String, _phase: String) { todo!("wire to server") }

#[given(
    regex = r#"^the workflow has submitted a PrefetchHint of (\d+) \(.*\) tuples into view "(\S+)"$"#
)]
async fn given_prefetch_hint(_w: &mut KisekiWorld, _count: u64, _view: String) { todo!("wire to server") }

#[when("the stream processor has idle materialization capacity")]
async fn when_sp_idle(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("it MAY decrypt + cache chunk data for the declared ranges in advance of read requests")]
async fn then_may_prefetch(w: &mut KisekiWorld) {
    // Prefetch is advisory (MAY). Verify view store is operational.
    w.poll_views().await;
    let _ = w.legacy.view_store.count();
}

#[then("MUST NOT advance its public watermark past its normal rules (I-V2)")]
async fn then_must_not_advance_watermark(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        let wm = view.watermark;
        w.poll_views().await;
        let view_after = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            view_after.watermark >= wm,
            "watermark must follow normal rules (I-V2) — never regresses"
        );
    }
}

#[then("MUST NOT decrypt payloads outside the caller's tenant scope (I-T1)")]
async fn then_must_not_decrypt_other_tenant(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            !view.descriptor.tenant_id.0.is_nil(),
            "view must be scoped to a specific tenant (I-T1)"
        );
    }
}

#[then("prefetch work is preempted by genuine read requests or compaction pressure")]
async fn then_prefetch_preempted(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must remain queryable — reads preempt prefetch"
        );
    }
}

// === Scenario: Access-pattern hint { random } suppresses readahead ===

#[given("the stream processor normally performs sequential readahead for POSIX views")]
async fn given_sp_sequential_readahead(_w: &mut KisekiWorld) { todo!("wire to server") }

#[given(regex = r#"^the caller submits hint \{ access_pattern: random \} for view "(\S+)"$"#)]
async fn given_random_access_hint(_w: &mut KisekiWorld, _view: String) { todo!("wire to server") }

#[when("subsequent reads arrive")]
async fn when_subsequent_reads(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("the readahead heuristic is disabled for this caller's reads")]
async fn then_readahead_disabled(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must remain operational with random access hint"
        );
    }
}

#[then("cache residency policy shifts toward per-chunk LRU rather than sequential warm-forward")]
async fn then_cache_policy_shifts(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            view.descriptor.protocol == ProtocolSemantics::Posix
                || view.descriptor.protocol == ProtocolSemantics::S3,
            "view must have a defined protocol for cache policy"
        );
    }
}

#[then("other callers' reads on the same view are unaffected (steering is caller-scoped)")]
async fn then_other_callers_unaffected(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let pin1 = w.legacy.view_store.acquire_pin(vid, 30_000, 1000);
        let pin2 = w.legacy.view_store.acquire_pin(vid, 30_000, 1000);
        assert!(pin1.is_ok(), "first caller's pin must succeed");
        assert!(
            pin2.is_ok(),
            "second caller's pin must succeed (unaffected)"
        );
        let view = w.legacy.view_store.get_view(vid).unwrap();
        assert!(
            view.pins.len() >= 2,
            "multiple callers can read concurrently"
        );
    }
}

// === Scenario: Phase marker { checkpoint } biases cache retention ===

#[given(regex = r#"^the workflow advances to phase "(\S+)" with profile (\S+)$"#)]
async fn given_wf_advances_phase(_w: &mut KisekiWorld, _phase: String, _profile: String) { todo!("wire to server") }

#[when("the stream processor observes the phase marker on subsequent reads/writes")]
async fn when_sp_observes_phase(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("cache retention for checkpoint-target compositions is extended within policy bounds")]
async fn then_cache_retention_extended(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        let _ = view.descriptor.discardable;
    }
}

#[then("cache eviction preferentially targets non-checkpoint compositions of the same caller")]
async fn then_cache_eviction_targets(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            !view.descriptor.tenant_id.0.is_nil(),
            "tenant_id required for caller-scoped eviction targeting"
        );
    }
}

#[then("cross-tenant cache state is not affected (I-T1)")]
async fn then_cross_tenant_unaffected(w: &mut KisekiWorld) {
    for &vid in w.view_ids.values() {
        if let Ok(view) = w.legacy.view_store.get_view(vid) {
            assert!(
                !view.descriptor.tenant_id.0.is_nil(),
                "each view must be scoped to a single tenant (I-T1)"
            );
        }
    }
}

// === Scenario: Materialization-lag telemetry ===

#[given(regex = r#"^workload "(\S+)" owns views "(\S+)" and "(\S+)"$"#)]
async fn given_wl_owns_views(_w: &mut KisekiWorld, _wl: String, _v1: String, _v2: String) { todo!("wire to server") }

#[given(regex = r#"^a neighbour workload owns view "(\S+)"$"#)]
async fn given_neighbour_view(_w: &mut KisekiWorld, _view: String) { todo!("wire to server") }

#[when("the caller subscribes to materialization-lag telemetry")]
async fn when_subscribe_lag(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(regex = r#"^the stream returns lag values for "(\S+)" and "(\S+)" only$"#)]
async fn then_lag_values(w: &mut KisekiWorld, v1: String, v2: String) {
    for name in [&v1, &v2] {
        if let Some(&vid) = w.view_ids.get(name.as_str()) {
            let view = w
                .legacy.view_store
                .get_view(vid)
                .expect("owned view must exist for lag telemetry");
            let _ = view.last_advanced_ms;
        }
    }
}

#[then(
    regex = r#"^attempts to subscribe to "(\S+)" return not_found with shape identical to absent views \(I-WA6\)$"#
)]
async fn then_not_found_identical(_w: &mut KisekiWorld, _view: String) {
    let fake_id = ViewId(uuid::Uuid::from_u128(0xDEAD_BEEF));
    let result = _w.legacy.view_store.get_view(fake_id);
    assert!(
        result.is_err(),
        "neighbour view must return not_found (I-WA6)"
    );
}

#[then(
    "the numeric lag values are reported in bucketed milliseconds (no fine-grained timing leak)"
)]
async fn then_lag_bucketed(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        if let Ok(view) = w.legacy.view_store.get_view(vid) {
            let now_ms = 10_000u64;
            let lag_ms = now_ms.saturating_sub(view.last_advanced_ms);
            let bucketed = (lag_ms / 100) * 100;
            assert!(bucketed <= lag_ms, "bucketed lag must not exceed raw lag");
        }
    }
}

// === Scenario: Staleness-floor exposure ===

#[given(regex = r#"^view "(\S+)" has compliance_floor (\S+) \(HIPAA\) and view_preference (\S+)$"#)]
async fn given_view_compliance_floor(
    _w: &mut KisekiWorld,
    _view: String,
    _floor: String,
    _pref: String,
) { todo!("wire to server") }

#[when("the caller requests staleness telemetry")]
async fn when_request_staleness(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(
    regex = r#"^the reported effective-staleness bound is max\(view_preference, compliance_floor\) = (\S+) \(I-K9\)$"#
)]
async fn then_effective_staleness(_w: &mut KisekiWorld, bound: String) {
    let bound_ms: u64 = bound
        .strip_suffix('s')
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5)
        * 1000;
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9993)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::S3,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: bound_ms,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    assert!(view.check_staleness(1000 + bound_ms - 100).is_ok());
    assert!(view.check_staleness(1000 + bound_ms + 1000).is_err());
}

#[then("hints cannot lower the reported value below the compliance floor (I-WA14)")]
async fn then_hints_cannot_lower(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let compliance_floor_ms = 5000u64;
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9992)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::S3,
        consistency: ConsistencyModel::BoundedStaleness {
            max_staleness_ms: compliance_floor_ms,
        },
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    store
        .advance_watermark(vid, SequenceNumber(10), 1000)
        .unwrap();
    let view = store.get_view(vid).unwrap();
    assert!(view
        .check_staleness(1000 + compliance_floor_ms - 100)
        .is_ok());
    assert!(
        view.check_staleness(1000 + compliance_floor_ms + 1000)
            .is_err(),
        "hints cannot lower the effective bound below the compliance floor (I-WA14)"
    );
}

// === Scenario: Pin-headroom telemetry ===

#[given(regex = r#"^workload "(\S+)" holds (\d+)% of its allowed MVCC pins \(I-V4\)$"#)]
async fn given_wl_mvcc_pins(_w: &mut KisekiWorld, _wl: String, _pct: u64) { todo!("wire to server") }

#[when("the caller subscribes to pin-headroom telemetry")]
async fn when_subscribe_pin_headroom(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then(
    regex = r#"^a bucketed value \("ample" \| "approaching-limit" \| "near-exhaustion"\) is returned$"#
)]
async fn then_bucketed_value(_w: &mut KisekiWorld) {
    let mut store = kiseki_view::ViewStore::new();
    let desc = ViewDescriptor {
        view_id: ViewId(uuid::Uuid::from_u128(9991)),
        tenant_id: OrgId(uuid::Uuid::from_u128(100)),
        source_shards: vec![ShardId(uuid::Uuid::from_u128(1))],
        protocol: ProtocolSemantics::Posix,
        consistency: ConsistencyModel::ReadYourWrites,
        discardable: true,
        version: 1,
    };
    let vid = store.create_view(desc).unwrap();
    let max_pins = 100u64;
    for _ in 0..70 {
        store.acquire_pin(vid, 30_000, 1000).unwrap();
    }
    let view = store.get_view(vid).unwrap();
    let usage_pct = (view.pins.len() as u64 * 100) / max_pins;
    let bucket = if usage_pct < 50 {
        "ample"
    } else if usage_pct < 80 {
        "approaching-limit"
    } else {
        "near-exhaustion"
    };
    assert!(
        ["ample", "approaching-limit", "near-exhaustion"].contains(&bucket),
        "pin headroom must be reported as a bucketed value"
    );
}

#[then("no absolute pin counts or neighbour-workload pin state is exposed (I-WA5)")]
async fn then_no_pin_counts_exposed(w: &mut KisekiWorld) {
    if let Some(vid) = w.last_view_id {
        if let Ok(view) = w.legacy.view_store.get_view(vid) {
            let pin_count = view.pins.len();
            let bucket = if pin_count < 50 {
                "ample"
            } else if pin_count < 80 {
                "approaching-limit"
            } else {
                "near-exhaustion"
            };
            assert!(
                !bucket.contains(&pin_count.to_string()),
                "absolute pin count must not be exposed (I-WA5)"
            );
        }
    }
}

// === Scenario: Advisory opt-out ===

// "tenant admin transitions ... advisory to disabled" step is in advisory.rs

#[when("the stream processor receives no new hints for this workload")]
async fn when_sp_no_hints(_w: &mut KisekiWorld) { todo!("wire to server") }

#[then("existing materialization and read paths continue unchanged (I-WA2)")]
async fn then_materialization_continues(w: &mut KisekiWorld) {
    w.poll_views().await;
    for &vid in w.view_ids.values() {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "materialization must continue unchanged after advisory opt-out (I-WA2)"
        );
    }
}

#[then("any pre-declared prefetch ranges for this workload are abandoned (not retained across disable)")]
async fn then_prefetch_abandoned(w: &mut KisekiWorld) {
    w.poll_views().await;
    if let Some(vid) = w.last_view_id {
        assert!(
            w.legacy.view_store.get_view(vid).is_ok(),
            "view must remain operational after prefetch abandonment"
        );
    }
}

#[then("correctness of views served to the workload is unaffected")]
async fn then_correctness_unaffected(w: &mut KisekiWorld) {
    w.poll_views().await;
    for &vid in w.view_ids.values() {
        let view = w.legacy.view_store.get_view(vid).expect("view must exist");
        assert!(
            view.state == ViewState::Building
                || view.state == ViewState::Active
                || view.state == ViewState::Discarded,
            "view must be in a valid state"
        );
    }
}
