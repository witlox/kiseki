//! openraft integration for the Log context.
//!
//! Per-shard Raft groups. Each shard gets its own log store and
//! state machine. Pattern follows `kiseki-keymanager/src/raft/`.

#[allow(missing_docs)]
pub mod log_store;
#[allow(missing_docs)]
pub mod network;
#[allow(missing_docs)]
pub mod openraft_store;
#[allow(missing_docs)]
pub mod state_machine;
pub mod test_cluster;
pub mod types;

pub use log_store::{ShardMemLogStore, ShardRedbLogStore};
pub use network::StubNetworkFactory;
pub use openraft_store::OpenRaftLogStore;
pub use state_machine::ShardStateMachine;
pub use types::LogTypeConfig;
