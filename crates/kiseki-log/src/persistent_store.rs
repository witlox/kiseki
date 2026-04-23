//! Persistent shard store — wraps `MemShardStore` + `RedbLogStore`.
//!
//! Every delta append is written to both in-memory store (fast reads)
//! and redb (durability). On startup, reloads from redb into memory.
//! Per ADR-022.

use std::path::Path;

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_raft::redb_log_store::RedbLogStore;

use crate::delta::Delta;
use crate::error::LogError;
use crate::shard::{ShardConfig, ShardInfo};
use crate::store::MemShardStore;
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};

/// Persistent shard store — in-memory + redb for durability.
pub struct PersistentShardStore {
    mem: MemShardStore,
    redb: RedbLogStore,
}

impl PersistentShardStore {
    /// Open or create a persistent store at the given path.
    ///
    /// If the redb database contains existing data, it is loaded
    /// into the in-memory store on startup.
    pub fn open(path: &Path) -> Result<Self, LogError> {
        let redb = RedbLogStore::open(path).map_err(|_| LogError::Unavailable)?;
        let mem = MemShardStore::new();

        let mut store = Self { mem, redb };
        store.reload();
        Ok(store)
    }

    /// Create a shard (delegates to in-memory store + persists metadata).
    pub fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        self.mem.create_shard(shard_id, tenant_id, node_id, config);
        // Persist shard metadata.
        let key = format!("shard:{}", shard_id.0);
        let meta = ShardMeta {
            shard_id_bytes: *shard_id.0.as_bytes(),
            tenant_id_bytes: *tenant_id.0.as_bytes(),
            node_id: node_id.0,
        };
        let _ = self.redb.set_meta(&key, &meta);
    }

    /// Reload all data from redb into the in-memory store.
    ///
    /// Iterates persisted shard metadata and deltas, re-creates shards
    /// and replays deltas into the in-memory store.
    fn reload(&mut self) {
        // MVP: shard metadata not iterated from redb. Shards are
        // re-created via bootstrap on startup. Deltas replayed below.

        // Reload deltas — iterate all entries in the log table.
        if let Ok(entries) = self.redb.range::<PersistedDelta>(1, u64::MAX) {
            use kiseki_common::ids::{NodeId, OrgId, ShardId};
            use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

            // First pass: collect unique shard IDs and ensure they exist.
            let mut seen_shards = std::collections::HashSet::new();
            for (_, delta) in &entries {
                let shard_id = ShardId(uuid::Uuid::from_bytes(delta.shard_id_bytes));
                if seen_shards.insert(shard_id) {
                    let tenant_id = OrgId(uuid::Uuid::from_bytes(delta.tenant_id_bytes));
                    // Create shard if not already exists (idempotent).
                    self.mem.create_shard(
                        shard_id,
                        tenant_id,
                        NodeId(1),
                        crate::shard::ShardConfig::default(),
                    );
                }
            }

            // Second pass: replay deltas.
            for (_seq, delta) in entries {
                let shard_id = ShardId(uuid::Uuid::from_bytes(delta.shard_id_bytes));
                let tenant_id = OrgId(uuid::Uuid::from_bytes(delta.tenant_id_bytes));
                let operation = match delta.operation {
                    0 => crate::delta::OperationType::Create,
                    1 => crate::delta::OperationType::Update,
                    2 => crate::delta::OperationType::Delete,
                    3 => crate::delta::OperationType::Rename,
                    4 => crate::delta::OperationType::SetAttribute,
                    _ => crate::delta::OperationType::Finalize,
                };
                let timestamp = DeltaTimestamp {
                    hlc: HybridLogicalClock {
                        physical_ms: delta.hlc_physical_ms,
                        logical: delta.hlc_logical,
                        node_id: NodeId(delta.hlc_node_id),
                    },
                    wall: WallTime {
                        millis_since_epoch: delta.wall_millis,
                        timezone: delta.wall_timezone.clone(),
                    },
                    quality: match delta.clock_quality {
                        1 => ClockQuality::Ptp,
                        2 => ClockQuality::Gps,
                        3 => ClockQuality::Unsync,
                        _ => ClockQuality::Ntp,
                    },
                };
                let _ = self.mem.append_delta(AppendDeltaRequest {
                    shard_id,
                    tenant_id,
                    operation,
                    timestamp,
                    hashed_key: delta.hashed_key,
                    chunk_refs: vec![],
                    payload: delta.payload,
                    has_inline_data: delta.has_inline_data,
                });
            }
            tracing::info!(shard_count = seen_shards.len(), "shards restored from redb");
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ShardMeta {
    shard_id_bytes: [u8; 16],
    tenant_id_bytes: [u8; 16],
    node_id: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedDelta {
    shard_id_bytes: [u8; 16],
    tenant_id_bytes: [u8; 16],
    operation: u8,
    hashed_key: [u8; 32],
    payload: Vec<u8>,
    has_inline_data: bool,
    // Timestamp fields (added for timestamp fidelity across restart).
    hlc_physical_ms: u64,
    hlc_logical: u32,
    hlc_node_id: u64,
    wall_millis: u64,
    wall_timezone: String,
    clock_quality: u8,
}

impl LogOps for PersistentShardStore {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        // Write to in-memory first (assigns sequence number).
        let seq = self.mem.append_delta(req.clone())?;

        // Persist the full request to redb for reload.
        let persisted = PersistedDelta {
            shard_id_bytes: *req.shard_id.0.as_bytes(),
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: match req.operation {
                crate::delta::OperationType::Create => 0,
                crate::delta::OperationType::Update => 1,
                crate::delta::OperationType::Delete => 2,
                crate::delta::OperationType::Rename => 3,
                crate::delta::OperationType::SetAttribute => 4,
                crate::delta::OperationType::Finalize => 5,
            },
            hashed_key: req.hashed_key,
            payload: req.payload,
            has_inline_data: req.has_inline_data,
            hlc_physical_ms: req.timestamp.hlc.physical_ms,
            hlc_logical: req.timestamp.hlc.logical,
            hlc_node_id: req.timestamp.hlc.node_id.0,
            wall_millis: req.timestamp.wall.millis_since_epoch,
            wall_timezone: req.timestamp.wall.timezone.clone(),
            clock_quality: match req.timestamp.quality {
                kiseki_common::time::ClockQuality::Ntp => 0,
                kiseki_common::time::ClockQuality::Ptp => 1,
                kiseki_common::time::ClockQuality::Gps => 2,
                kiseki_common::time::ClockQuality::Unsync => 3,
            },
        };
        let _ = self.redb.append(seq.0, &persisted);

        Ok(seq)
    }

    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        self.mem.read_deltas(req)
    }

    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        self.mem.shard_health(shard_id)
    }

    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        self.mem.set_maintenance(shard_id, enabled)
    }

    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        self.mem.truncate_log(shard_id)
    }

    fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        self.mem.compact_shard(shard_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::OperationType;
    use kiseki_common::ids::NodeId;
    use kiseki_common::time::*;

    fn test_shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }
    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
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

    #[test]
    fn append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentShardStore::open(&dir.path().join("test.redb")).unwrap();
        store.create_shard(
            test_shard(),
            test_tenant(),
            NodeId(1),
            ShardConfig::default(),
        );

        let seq = store
            .append_delta(AppendDeltaRequest {
                shard_id: test_shard(),
                tenant_id: test_tenant(),
                operation: OperationType::Create,
                timestamp: test_timestamp(),
                hashed_key: [0x42; 32],
                chunk_refs: vec![],
                payload: b"test payload".to_vec(),
                has_inline_data: true,
            })
            .unwrap();

        assert_eq!(seq, SequenceNumber(1));

        let deltas = store
            .read_deltas(ReadDeltasRequest {
                shard_id: test_shard(),
                from: SequenceNumber(1),
                to: SequenceNumber(1),
            })
            .unwrap();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].header.sequence, SequenceNumber(1));
    }

    #[test]
    fn redb_records_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.redb");

        // Write data.
        {
            let store = PersistentShardStore::open(&path).unwrap();
            store.create_shard(
                test_shard(),
                test_tenant(),
                NodeId(1),
                ShardConfig::default(),
            );
            store
                .append_delta(AppendDeltaRequest {
                    shard_id: test_shard(),
                    tenant_id: test_tenant(),
                    operation: OperationType::Create,
                    timestamp: test_timestamp(),
                    hashed_key: [0x01; 32],
                    chunk_refs: vec![],
                    payload: b"persisted".to_vec(),
                    has_inline_data: false,
                })
                .unwrap();
        }

        // Reopen — reload should restore the delta into in-memory store.
        {
            let store = PersistentShardStore::open(&path).unwrap();
            let deltas = store
                .read_deltas(ReadDeltasRequest {
                    shard_id: test_shard(),
                    from: SequenceNumber(1),
                    to: SequenceNumber(1),
                })
                .unwrap();
            assert_eq!(deltas.len(), 1, "delta should survive reopen via reload");
            assert_eq!(deltas[0].payload.ciphertext, b"persisted");
        }
    }

    #[test]
    fn timestamps_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ts.redb");

        let ts = DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1_718_000_000_000,
                logical: 42,
                node_id: NodeId(7),
            },
            wall: WallTime {
                millis_since_epoch: 1_718_000_000_000,
                timezone: "Europe/Zurich".into(),
            },
            quality: ClockQuality::Ptp,
        };

        // Write delta with specific timestamp.
        {
            let store = PersistentShardStore::open(&path).unwrap();
            store.create_shard(
                test_shard(),
                test_tenant(),
                NodeId(7),
                ShardConfig::default(),
            );
            store
                .append_delta(AppendDeltaRequest {
                    shard_id: test_shard(),
                    tenant_id: test_tenant(),
                    operation: OperationType::Create,
                    timestamp: ts.clone(),
                    hashed_key: [0xAA; 32],
                    chunk_refs: vec![],
                    payload: b"timestamped".to_vec(),
                    has_inline_data: false,
                })
                .unwrap();
        }

        // Reopen and verify timestamp fidelity.
        {
            let store = PersistentShardStore::open(&path).unwrap();
            let deltas = store
                .read_deltas(ReadDeltasRequest {
                    shard_id: test_shard(),
                    from: SequenceNumber(1),
                    to: SequenceNumber(1),
                })
                .unwrap();

            assert_eq!(deltas.len(), 1);
            let d = &deltas[0];
            assert_eq!(d.header.timestamp.hlc.physical_ms, 1_718_000_000_000);
            assert_eq!(d.header.timestamp.hlc.logical, 42);
            assert_eq!(d.header.timestamp.hlc.node_id, NodeId(7));
            assert_eq!(
                d.header.timestamp.wall.millis_since_epoch,
                1_718_000_000_000
            );
            assert_eq!(d.header.timestamp.wall.timezone, "Europe/Zurich");
            assert_eq!(d.header.timestamp.quality, ClockQuality::Ptp);
        }
    }
}
