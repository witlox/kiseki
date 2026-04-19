//! Re-export of generic log store for Log shard Raft groups.
pub use kiseki_raft::MemLogStore;

/// Type alias for the log shard's Raft log store.
pub type ShardLogStore = MemLogStore<super::types::LogTypeConfig>;
