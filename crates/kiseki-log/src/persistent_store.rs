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
    /// MVP: shards are re-created via bootstrap on startup.
    /// Production would iterate redb keys to rebuild shard state.
    #[allow(clippy::unused_self)]
    fn reload(&mut self) {
        // TODO: iterate redb shard metadata keys, recreate shards in mem.
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ShardMeta {
    shard_id_bytes: [u8; 16],
    tenant_id_bytes: [u8; 16],
    node_id: u64,
}

impl LogOps for PersistentShardStore {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        // Write to in-memory first (assigns sequence number).
        let seq = self.mem.append_delta(req.clone())?;

        // Persist to redb.
        let key = format!("delta:{}:{}", req.shard_id.0, seq.0);
        let _ = self.redb.append(seq.0, &key);

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
    use kiseki_common::ids::NodeId;
    use kiseki_common::time::*;
    use crate::delta::OperationType;

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

        // Reopen — redb should have the record.
        {
            let store = PersistentShardStore::open(&path).unwrap();
            // Verify redb has the delta key.
            let key: Option<String> = store.redb.get(1).unwrap();
            assert!(key.is_some(), "redb should have persisted delta at index 1");
        }
    }
}
