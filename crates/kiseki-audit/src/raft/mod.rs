//! openraft integration for the Audit context.
//!
//! Append-only state machine (I-A1). Per-tenant Raft groups.

#[allow(missing_docs)]
pub mod log_store;
#[allow(missing_docs)]
pub mod network;
#[allow(missing_docs)]
pub mod openraft_store;
#[allow(missing_docs)]
pub mod state_machine;
pub mod types;

pub use log_store::AuditLogStore;
pub use network::StubNetworkFactory;
pub use openraft_store::OpenRaftAuditStore;
pub use state_machine::AuditStateMachine;
pub use types::AuditTypeConfig;
