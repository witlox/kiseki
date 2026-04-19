//! Tests for log.feature scenarios against the in-memory shard store.
//!
//! Maps Gherkin scenarios to unit tests covering the `LogOps` semantics.

use kiseki_common::ids::*;
use kiseki_common::time::*;
use kiseki_log::delta::OperationType;
use kiseki_log::shard::{ShardConfig, ShardState};
use kiseki_log::store::MemShardStore;
use kiseki_log::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};

fn test_shard_id() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_node() -> NodeId {
    NodeId(1)
}

fn test_timestamp() -> DeltaTimestamp {
    DeltaTimestamp {
        hlc: HybridLogicalClock {
            physical_ms: 1000,
            logical: 0,
            node_id: NodeId(1),
        },
        wall: WallTime {
            millis_since_epoch: 1000,
            timezone: "UTC".into(),
        },
        quality: ClockQuality::Ntp,
    }
}

fn make_request(shard_id: ShardId, key_byte: u8) -> AppendDeltaRequest {
    AppendDeltaRequest {
        shard_id,
        tenant_id: test_tenant(),
        operation: OperationType::Create,
        timestamp: test_timestamp(),
        hashed_key: [key_byte; 32],
        chunk_refs: vec![],
        payload: vec![0xab; 64],
        has_inline_data: false,
    }
}

fn setup_store() -> MemShardStore {
    let store = MemShardStore::new();
    store.create_shard(
        test_shard_id(),
        test_tenant(),
        test_node(),
        ShardConfig::default(),
    );
    store
}

// --- Scenario: Successful delta append ---
#[test]
fn successful_delta_append() {
    let store = setup_store();
    let req = make_request(test_shard_id(), 0x50);

    let seq = store.append_delta(req);
    assert!(seq.is_ok());
    assert_eq!(seq.unwrap_or_else(|_| unreachable!()), SequenceNumber(1));
}

// --- Scenario: Delta with inline data below threshold ---
#[test]
fn inline_data_delta() {
    let store = setup_store();
    let mut req = make_request(test_shard_id(), 0x50);
    req.has_inline_data = true;
    req.payload = vec![0xcd; 1024]; // small inline data

    let seq = store.append_delta(req);
    assert!(seq.is_ok());

    let deltas = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard_id(),
            from: SequenceNumber(1),
            to: SequenceNumber(1),
        })
        .unwrap_or_else(|_| unreachable!());

    assert_eq!(deltas.len(), 1);
    assert!(deltas[0].header.has_inline_data);
}

// --- Scenario: Deltas maintain total order within shard ---
#[test]
fn total_order_within_shard() {
    let store = setup_store();

    let seq1 = store
        .append_delta(make_request(test_shard_id(), 0x10))
        .unwrap_or_else(|_| unreachable!());
    let seq2 = store
        .append_delta(make_request(test_shard_id(), 0x20))
        .unwrap_or_else(|_| unreachable!());
    let seq3 = store
        .append_delta(make_request(test_shard_id(), 0x30))
        .unwrap_or_else(|_| unreachable!());

    // Monotonic, gap-free (I-L1).
    assert_eq!(seq1, SequenceNumber(1));
    assert_eq!(seq2, SequenceNumber(2));
    assert_eq!(seq3, SequenceNumber(3));

    // Read all: total order preserved.
    let deltas = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard_id(),
            from: SequenceNumber(1),
            to: SequenceNumber(3),
        })
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0].header.sequence, SequenceNumber(1));
    assert_eq!(deltas[1].header.sequence, SequenceNumber(2));
    assert_eq!(deltas[2].header.sequence, SequenceNumber(3));
}

// --- Scenario: Maintenance mode rejects writes ---
#[test]
fn maintenance_mode_rejects_writes() {
    let store = setup_store();
    store
        .set_maintenance(test_shard_id(), true)
        .unwrap_or_else(|_| unreachable!());

    let result = store.append_delta(make_request(test_shard_id(), 0x50));
    assert!(result.is_err());

    // Reads still work.
    let health = store.shard_health(test_shard_id());
    assert!(health.is_ok());
    assert_eq!(
        health.unwrap_or_else(|_| unreachable!()).state,
        ShardState::Maintenance
    );
}

// --- Scenario: Exiting maintenance mode resumes writes ---
#[test]
fn exit_maintenance_resumes_writes() {
    let store = setup_store();
    store
        .set_maintenance(test_shard_id(), true)
        .unwrap_or_else(|_| unreachable!());
    store
        .set_maintenance(test_shard_id(), false)
        .unwrap_or_else(|_| unreachable!());

    let result = store.append_delta(make_request(test_shard_id(), 0x50));
    assert!(result.is_ok());
}

// --- Scenario: Delta GC respects all consumer watermarks ---
#[test]
fn gc_respects_consumer_watermarks() {
    let store = setup_store();

    // Append 10 deltas.
    for i in 0u8..10 {
        let key = i * 10 + 10;
        store
            .append_delta(make_request(test_shard_id(), key))
            .unwrap_or_else(|_| unreachable!());
    }

    // Register consumers at different positions.
    store
        .register_consumer(test_shard_id(), "sp-nfs", SequenceNumber(0))
        .unwrap_or_else(|_| unreachable!());
    store
        .register_consumer(test_shard_id(), "sp-s3", SequenceNumber(0))
        .unwrap_or_else(|_| unreachable!());
    store
        .advance_watermark(test_shard_id(), "sp-nfs", SequenceNumber(8))
        .unwrap_or_else(|_| unreachable!());
    store
        .advance_watermark(test_shard_id(), "sp-s3", SequenceNumber(5))
        .unwrap_or_else(|_| unreachable!());

    // GC boundary should be min(8, 5) = 5.
    let boundary = store
        .truncate_log(test_shard_id())
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(boundary, SequenceNumber(5));

    // Deltas 1-4 should be GC'd, 5-10 retained.
    let remaining = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard_id(),
            from: SequenceNumber(1),
            to: SequenceNumber(10),
        })
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(remaining.len(), 6); // sequences 5,6,7,8,9,10
    assert_eq!(remaining[0].header.sequence, SequenceNumber(5));
}

// --- Scenario: Stream processor reads delta range ---
#[test]
fn read_delta_range() {
    let store = setup_store();
    for i in 0u8..20 {
        let key = i * 10 + 10;
        store
            .append_delta(make_request(test_shard_id(), key))
            .unwrap_or_else(|_| unreachable!());
    }

    let deltas = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard_id(),
            from: SequenceNumber(10),
            to: SequenceNumber(15),
        })
        .unwrap_or_else(|_| unreachable!());

    assert_eq!(deltas.len(), 6);
    for (i, delta) in deltas.iter().enumerate() {
        assert_eq!(delta.header.sequence, SequenceNumber(10 + i as u64));
    }
}

// --- Scenario: Shard health reporting ---
#[test]
fn shard_health_reports_state() {
    let store = setup_store();
    store
        .append_delta(make_request(test_shard_id(), 0x50))
        .unwrap_or_else(|_| unreachable!());

    let info = store
        .shard_health(test_shard_id())
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(info.state, ShardState::Healthy);
    assert_eq!(info.tip, SequenceNumber(1));
    assert_eq!(info.delta_count, 1);
}

// --- Scenario: Shard not found ---
#[test]
fn shard_not_found() {
    let store = MemShardStore::new();
    let result = store.shard_health(ShardId(uuid::Uuid::from_u128(999)));
    assert!(result.is_err());
}

// --- Scenario: Shard split ---
#[test]
fn shard_split_redistributes_deltas() {
    let store = MemShardStore::new();
    store.create_shard(
        test_shard_id(),
        test_tenant(),
        test_node(),
        ShardConfig {
            max_delta_count: 5,
            max_byte_size: u64::MAX,
        },
    );

    // Append deltas with diverse keys.
    for i in 0u8..6 {
        store
            .append_delta(make_request(test_shard_id(), i * 40 + 10))
            .unwrap_or_else(|_| unreachable!());
    }

    assert!(store.should_split(test_shard_id()));

    let new_id = ShardId(uuid::Uuid::from_u128(2));
    let result = store.split_shard(test_shard_id(), new_id, test_node());
    assert!(result.is_ok());

    // Both shards should exist and have deltas partitioned by key range.
    let old_info = store
        .shard_health(test_shard_id())
        .unwrap_or_else(|_| unreachable!());
    let new_info = store
        .shard_health(new_id)
        .unwrap_or_else(|_| unreachable!());

    // Total deltas should equal original count.
    assert_eq!(old_info.delta_count + new_info.delta_count, 6);
}

// --- Scenario: Key out of range rejected ---
#[test]
fn key_out_of_range_rejected() {
    let store = MemShardStore::new();
    let shard_id = test_shard_id();
    store.create_shard(shard_id, test_tenant(), test_node(), ShardConfig::default());

    // After a split, try to append to the wrong shard.
    let new_id = ShardId(uuid::Uuid::from_u128(2));
    store
        .split_shard(shard_id, new_id, test_node())
        .unwrap_or_else(|_| unreachable!());

    // The original shard now covers [0x00, midpoint).
    // A key at 0xFF should be out of range.
    let result = store.append_delta(make_request(shard_id, 0xFF));
    assert!(result.is_err());
}

// --- Scenario: Automatic compaction merges SSTables ---
#[test]
fn compaction_keeps_latest_per_key() {
    let store = setup_store();

    // Append multiple deltas for the same hashed_key.
    let key = 0x50u8;
    for _ in 0..5 {
        store
            .append_delta(make_request(test_shard_id(), key))
            .unwrap_or_else(|_| unreachable!());
    }
    // Append one delta for a different key.
    store
        .append_delta(make_request(test_shard_id(), 0x60))
        .unwrap_or_else(|_| unreachable!());

    assert_eq!(
        store
            .shard_health(test_shard_id())
            .unwrap_or_else(|_| unreachable!())
            .delta_count,
        6
    );

    let removed = store
        .compact_shard(test_shard_id())
        .unwrap_or_else(|_| unreachable!());

    // Should keep 1 for key 0x50 (latest) + 1 for key 0x60 = 2 total.
    assert_eq!(removed, 4);
    assert_eq!(
        store
            .shard_health(test_shard_id())
            .unwrap_or_else(|_| unreachable!())
            .delta_count,
        2
    );
}

// --- Scenario: Compaction removes tombstones past watermark ---
#[test]
fn compaction_removes_old_tombstones() {
    let store = setup_store();

    // Append a create, then a delete (tombstone) for the same key.
    store
        .append_delta(make_request(test_shard_id(), 0x50))
        .unwrap_or_else(|_| unreachable!());
    let mut delete_req = make_request(test_shard_id(), 0x50);
    delete_req.operation = OperationType::Delete;
    store
        .append_delta(delete_req)
        .unwrap_or_else(|_| unreachable!());

    // Register a consumer that has consumed past both deltas.
    store
        .register_consumer(test_shard_id(), "sp-nfs", SequenceNumber(0))
        .unwrap_or_else(|_| unreachable!());
    store
        .advance_watermark(test_shard_id(), "sp-nfs", SequenceNumber(3))
        .unwrap_or_else(|_| unreachable!());

    let removed = store
        .compact_shard(test_shard_id())
        .unwrap_or_else(|_| unreachable!());

    // Tombstone for key 0x50 is the latest, but it's past watermark → removed.
    // The create is superseded by the delete → removed.
    assert_eq!(removed, 2);
}
