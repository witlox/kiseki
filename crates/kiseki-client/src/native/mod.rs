//! Native gRPC client (ADR-042 Phase 5).
//!
//! ```text
//!  NativeClient ──┬─► tonic::transport::Channel
//!                 ├─► TopologyCache    (RouteHit → leader node)
//!                 ├─► LeaseManager     (active leases + fencing)
//!                 └─► StreamCapMap     (per-tenant in-flight RAII)
//! ```
//!
//! See `specs/implementation/adr-042-native-gateway.md` Phase 5 for
//! the per-module contract.

pub mod client;
pub mod lease_manager;
pub mod stream_slot;
pub mod topology_cache;

pub use client::{NativeClient, NativeClientError};
pub use lease_manager::{ClientLease, LeaseManager};
pub use stream_slot::{StreamCapMap, StreamSlot};
pub use topology_cache::{
    Node, RefreshDecision, RouteHit, Shard, Snapshot, TopologyCache,
};
