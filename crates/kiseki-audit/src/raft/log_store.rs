//! Re-export of generic log store for Audit shard Raft groups.
pub use kiseki_raft::MemLogStore;

/// Type alias for the audit shard's Raft log store.
pub type AuditLogStore = MemLogStore<super::types::AuditTypeConfig>;
