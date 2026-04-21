//! Log context for Kiseki.
//!
//! The Log accepts deltas from the Composition context, assigns total
//! ordering within a shard via Raft, replicates for durability, and
//! manages shard lifecycle (split, compaction, truncation).
//!
//! Invariant mapping:
//!   - I-L1 — total order within a shard (Raft sequence numbers)
//!   - I-L2 — durable on majority before ack (Raft commit)
//!   - I-L3 — delta immutable once committed (append-only store)
//!   - I-L4 — GC requires all consumers advanced (watermark check)
//!   - I-L6 — hard shard ceiling triggers split
//!   - I-L7 — header/payload structural separation

#![deny(unsafe_code)]

pub mod auto_split;
pub mod compaction_worker;
pub mod delta;
pub mod error;
pub mod grpc;
pub mod persistent_store;
pub mod raft;
pub mod raft_store;
pub mod shard;
pub mod store;
pub mod traits;
pub mod watermark;

pub use delta::{Delta, DeltaHeader, DeltaPayload, OperationType};
pub use error::LogError;
pub use raft_store::RaftLogStore;
pub use shard::{ShardConfig, ShardInfo, ShardState};
pub use store::MemShardStore;
pub use traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};
pub use watermark::ConsumerWatermarks;
