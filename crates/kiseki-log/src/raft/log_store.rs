//! Re-export of log store types for Log shard Raft groups.
pub use kiseki_raft::MemLogStore;
pub use kiseki_raft::RedbRaftLogStore;

/// Type alias for the in-memory log store (used when no `data_dir`).
pub type ShardMemLogStore = MemLogStore<super::types::LogTypeConfig>;

/// Type alias for the persistent log store (used when `data_dir` is set).
pub type ShardRedbLogStore = RedbRaftLogStore<super::types::LogTypeConfig>;
