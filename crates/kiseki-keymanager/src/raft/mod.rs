//! openraft integration for the key manager.
//!
//! Separates log storage (`KeyLogStore`) from state machine
//! (`KeyStateMachine`) per openraft architecture. Both are in-memory
//! for now — production will add persistence.

// Internal raft plumbing — docs on pub items only.
#[allow(missing_docs)]
pub mod log_store;
#[allow(missing_docs)]
pub mod network;
#[allow(missing_docs)]
pub mod openraft_store;
#[allow(missing_docs)]
pub mod state_machine;
pub mod types;

pub use log_store::KeyLogStore;
pub use network::StubNetworkFactory;
pub use openraft_store::OpenRaftKeyStore;
pub use state_machine::KeyStateMachine;
pub use types::KeyTypeConfig;
