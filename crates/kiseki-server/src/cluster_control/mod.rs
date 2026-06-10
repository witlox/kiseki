//! Control-plane Raft group — cluster-wide consensus on the
//! namespace shard map (ADR-033 §4).
//!
//! Every node in the cluster runs one instance of the control-plane
//! Raft group at a well-known group id (`CONTROL_RAFT_GROUP_ID`). The
//! group's state machine holds a `HashMap<namespace_id,
//! NamespaceShardMap>` — the canonical truth for which shards exist
//! per namespace, their key ranges, and their lifecycle state.
//!
//! Mutations to topology (create namespace, record split, record
//! merge, retire shard) are submitted as `ControlCommand`s through
//! `client_write` on the leader and replicated to every follower.
//! Each node's apply hook runs deterministically — the same sequence
//! of commands produces the same `HashMap` state on every replica.
//!
//! **Apply-side fan-out into `RaftShardStore`.** When a node applies
//! `ControlCommand::RecordSplit { new_shard_id, .. }`, it locally
//! triggers creation of the per-shard Raft group for `new_shard_id`
//! on this node. That fixes the multi-node split limitation that
//! ADR-033 §3 surfaces: pre-#4 the new shard's Raft group only
//! existed on the calling node; post-#4 it exists on every replica.
//!
//! Pattern follows `kiseki-keymanager::raft` and
//! `kiseki-audit::raft`: a single well-known group id, registered
//! with the multiplexed Raft transport (ADR-041), with `MemLogStore`
//! / `FjallRaftLogStore` selected by whether `KISEKI_DATA_DIR` is set.

#[allow(missing_docs)] // internal Raft plumbing
pub mod commands;
pub mod metrics;
#[allow(missing_docs)]
pub mod state_machine;
#[allow(missing_docs)]
pub mod store;
#[allow(missing_docs)]
pub mod types;

// ADR-049 phase 2: per-node device discovery + InventoryReporter.
pub mod device_discovery;

// ADR-049 phase 3: placement + capacity resolver (§D4.5 formula).
pub mod resolver;

// ADR-049 phase 5a (pure): kiseki-tier-paths.json pointer file +
// I-CP-Move enforcement. Wired into the runtime.rs boot path
// alongside the boot reorder (phase 5a continued).
pub mod tier_paths;

// ADR-049 phase 5a continued: boot-time integration helper.
// Wraps discovery → publish → resolve → I-CP-Move → save-pointer
// in one call.
pub mod phase5_boot;

// ADR-049 phase 5a continued (runtime read): bridge the
// `kiseki-tier-paths.json` pointer to the four fjall keyspace opens
// in runtime.rs (`SmallObject`, `IntentStore`, `CompositionMeta`,
// `ChunkMeta`). First boot falls back to `<data_dir>/<convention>`;
// Nth boot uses the resolved mount the prior boot recorded.
pub mod boot_paths;

pub use commands::ControlCommand;
pub use metrics::ClusterControlMetrics;
// Re-exports kept for the follow-up subtask that switches admin
// RPCs over to control-plane consensus and adds external readers
// of the namespace shard map. Marked `dead_code`-friendly via
// `#[allow]` because the apply-hook + non-blocking-create_shard
// piece is still pending.
#[allow(unused_imports)]
pub use state_machine::{ControlStateMachine, NamespaceShardMapSnapshot};
#[allow(unused_imports)]
pub use store::{
    ApplyHook, NamespaceRegistrar, NoopApplyHook, OpenRaftControlStore, ShardStoreApplyHook,
    CONTROL_RAFT_GROUP_ID,
};
#[allow(unused_imports)]
pub use types::{ControlResponse, ControlTypeConfig};
