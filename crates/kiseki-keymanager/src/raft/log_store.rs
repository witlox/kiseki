//! Re-export of generic log store for key manager Raft group.
pub use kiseki_raft::MemLogStore;

/// Type alias for the key manager's log store.
pub type KeyLogStore = MemLogStore<super::types::KeyTypeConfig>;
