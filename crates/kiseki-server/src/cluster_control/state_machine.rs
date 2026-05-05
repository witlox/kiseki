//! State machine for the control-plane Raft group (ADR-033 §4).
//!
//! Holds `HashMap<namespace_id, NamespaceShardMapSnapshot>` — a
//! deterministic projection of the namespace topology that every
//! replica converges on. The snapshot type intentionally lives next
//! to the state machine (not in `kiseki-control`) because the apply
//! hook serializes it onto the Raft log; reusing
//! `kiseki_control::shard_topology::NamespaceShardMap` would couple
//! the dependency-firewalled control crate to openraft (which it
//! must not depend on per the ADV-3 firewall comment in
//! `kiseki-control/Cargo.toml`).
//!
//! `NamespaceShardMapSnapshot` is a one-to-one reflection of the
//! production shape — gateway routing code can convert a snapshot
//! back into `NamespaceShardMap` via `to_topology()` without
//! semantic loss.

use std::collections::HashMap;
use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_control::shard_topology::{NamespaceCreationState, NamespaceShardMap, ShardRange};
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder};
use serde::{Deserialize, Serialize};

use super::commands::{ControlCommand, ShardRecord};
use super::store::ApplyHook;
use super::types::{ControlResponse, ControlTypeConfig};

type C = ControlTypeConfig;

/// One namespace's shard map as carried in the control-plane Raft
/// state machine. Mirrors `kiseki_control::NamespaceShardMap` but
/// owns its own serde derives so it can ride the Raft log + snapshot
/// stream without imposing those on the dependency-firewalled
/// `kiseki-control` crate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamespaceShardMapSnapshot {
    /// Stable namespace identifier.
    pub namespace_id: String,
    /// Owning tenant.
    pub tenant_id: OrgId,
    /// Monotonically increasing version on every successful apply.
    /// Useful for cache-invalidation: a routing-cache entry whose
    /// observed version is less than the state machine's is stale.
    pub version: u64,
    /// One per shard. Sorted by `range_start`. May contain a
    /// retired-but-not-yet-removed shard (post-merge) until the
    /// follow-up `RetireShard` command lands.
    pub shards: Vec<ShardSnapshot>,
}

/// One shard's record. `is_retiring` marks the post-merge state
/// where the shard's range has already been absorbed into the
/// surviving shard but the shard hasn't been finalized for removal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShardSnapshot {
    /// Stable shard id.
    pub shard_id: ShardId,
    /// Inclusive lower bound of the 256-bit key range.
    pub range_start: [u8; 32],
    /// Exclusive upper bound of the 256-bit key range.
    pub range_end: [u8; 32],
    /// Best-effort leader placement.
    pub leader_node: NodeId,
    /// Set on `RecordMerge` for the retired side. Cleared by
    /// `RetireShard` (which removes the shard entirely).
    pub is_retiring: bool,
}

impl NamespaceShardMapSnapshot {
    /// Convert this snapshot to the production
    /// `kiseki_control::NamespaceShardMap`. Caller-side use only —
    /// the conversion is lossy on `is_retiring` (the production type
    /// has no equivalent today; gateways treat retiring shards as
    /// regular shards for routing during the drain window).
    #[must_use]
    #[allow(dead_code)] // wired when admin RPC reads land
    pub fn to_topology(&self) -> NamespaceShardMap {
        NamespaceShardMap {
            namespace_id: self.namespace_id.clone(),
            tenant_id: self.tenant_id,
            version: self.version,
            shards: self
                .shards
                .iter()
                .map(|s| ShardRange {
                    shard_id: s.shard_id,
                    range_start: s.range_start,
                    range_end: s.range_end,
                    leader_node: s.leader_node,
                })
                .collect(),
            // Once the namespace exists in the control-plane Raft
            // state machine it is by definition Active — partial
            // creates are rolled back by the leader before the
            // CreateNamespace command commits (ADV-033-1).
            state: NamespaceCreationState::Active,
        }
    }
}

/// One snapshot of the entire control-plane state — used both for
/// openraft's snapshot install/build path and for unit tests that
/// want to deterministically inspect the post-apply state.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    /// All namespaces known to the control plane.
    pub namespaces: HashMap<String, NamespaceShardMapSnapshot>,
}

/// Inner state shared between the `RaftStateMachine` impl and
/// outside readers (e.g. routing). Wrapped in
/// `futures::lock::Mutex` for compatibility with the openraft
/// `apply` future.
pub(crate) struct StateMachineInner {
    pub(crate) namespaces: HashMap<String, NamespaceShardMapSnapshot>,
    pub(crate) last_applied_log: Option<LogIdOf<C>>,
    pub(crate) last_membership: StoredMembershipOf<C>,
}

impl StateMachineInner {
    pub(crate) fn new() -> Self {
        Self {
            namespaces: HashMap::new(),
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
        }
    }

    #[allow(clippy::too_many_lines)] // 4 idempotent variants — splitting hides the parallel structure
    fn apply_command(&mut self, cmd: &ControlCommand) -> ControlResponse {
        match cmd {
            ControlCommand::CreateNamespace {
                namespace_id,
                tenant_id,
                shards,
            } => {
                if self.namespaces.contains_key(namespace_id) {
                    // Idempotent on replay: ignore re-creation. The
                    // count returned matches what's already in the
                    // map so admin RPCs see a stable response.
                    let existing_count = u32::try_from(self.namespaces[namespace_id].shards.len())
                        .unwrap_or(u32::MAX);
                    return ControlResponse::NamespaceCreated {
                        shard_count: existing_count,
                    };
                }
                let mut shards: Vec<ShardSnapshot> =
                    shards.iter().map(shard_record_to_snapshot).collect();
                shards.sort_by_key(|s| s.range_start);
                let count = u32::try_from(shards.len()).unwrap_or(u32::MAX);
                self.namespaces.insert(
                    namespace_id.clone(),
                    NamespaceShardMapSnapshot {
                        namespace_id: namespace_id.clone(),
                        tenant_id: *tenant_id,
                        version: 1,
                        shards,
                    },
                );
                ControlResponse::NamespaceCreated { shard_count: count }
            }
            ControlCommand::RecordSplit {
                namespace_id,
                source_shard_id,
                new_shard_id,
                midpoint,
                new_leader,
            } => {
                let Some(map) = self.namespaces.get_mut(namespace_id) else {
                    // Idempotent: unknown namespace is a no-op (it
                    // may have been retired by a later command).
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                };
                // Idempotent on replay: if the new shard already
                // exists, skip the mutation.
                if map.shards.iter().any(|s| s.shard_id == *new_shard_id) {
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                }
                let Some(idx) = map
                    .shards
                    .iter()
                    .position(|s| s.shard_id == *source_shard_id)
                else {
                    // Source not found — possibly already split
                    // (idempotent) or never existed; accept either
                    // way to keep apply deterministic on replay.
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                };
                let source = &mut map.shards[idx];
                let upper_end = source.range_end;
                source.range_end = *midpoint;
                let new_shard = ShardSnapshot {
                    shard_id: *new_shard_id,
                    range_start: *midpoint,
                    range_end: upper_end,
                    leader_node: *new_leader,
                    is_retiring: false,
                };
                map.shards.push(new_shard);
                map.shards.sort_by_key(|s| s.range_start);
                map.version += 1;
                ControlResponse::SplitRecorded {
                    new_shard_id: *new_shard_id,
                }
            }
            ControlCommand::RecordMerge {
                namespace_id,
                surviving_shard_id,
                retired_shard_id,
                new_range_start,
                new_range_end,
            } => {
                let Some(map) = self.namespaces.get_mut(namespace_id) else {
                    return ControlResponse::Applied;
                };
                if let Some(s) = map
                    .shards
                    .iter_mut()
                    .find(|s| s.shard_id == *surviving_shard_id)
                {
                    s.range_start = *new_range_start;
                    s.range_end = *new_range_end;
                }
                if let Some(s) = map
                    .shards
                    .iter_mut()
                    .find(|s| s.shard_id == *retired_shard_id)
                {
                    s.is_retiring = true;
                }
                map.shards.sort_by_key(|s| s.range_start);
                map.version += 1;
                ControlResponse::Applied
            }
            ControlCommand::RetireShard {
                namespace_id,
                shard_id,
            } => {
                if let Some(map) = self.namespaces.get_mut(namespace_id) {
                    let before = map.shards.len();
                    map.shards.retain(|s| s.shard_id != *shard_id);
                    if map.shards.len() != before {
                        map.version += 1;
                    }
                }
                ControlResponse::Applied
            }
        }
    }
}

fn shard_record_to_snapshot(r: &ShardRecord) -> ShardSnapshot {
    ShardSnapshot {
        shard_id: r.shard_id,
        range_start: r.range_start,
        range_end: r.range_end,
        leader_node: r.leader_node,
        is_retiring: false,
    }
}

/// openraft state machine implementation for the control-plane group.
///
/// Holds the apply hook so it fires on every node from `apply()` —
/// not only on the leader. That is what makes ADR-033 §4 cluster-
/// wide: a single consensus event drives per-shard Raft group
/// creation on every replica, deterministically and in lockstep
/// with the namespace shard map mutation.
#[derive(Clone)]
pub struct ControlStateMachine {
    pub(crate) inner: Arc<futures::lock::Mutex<StateMachineInner>>,
    /// Apply hook fired AFTER each command has been applied to
    /// `inner`. `None` for unit tests that don't wire side effects.
    apply_hook: Option<Arc<dyn ApplyHook>>,
    /// Optional metrics: per-replica apply counts, namespace
    /// gauge, apply-hook duration. `None` for unit tests.
    metrics: Option<Arc<super::ClusterControlMetrics>>,
}

impl ControlStateMachine {
    /// Build a fresh state machine with no apply hook (tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(futures::lock::Mutex::new(StateMachineInner::new())),
            apply_hook: None,
            metrics: None,
        }
    }

    /// Build with an apply hook bound. The hook fires on every node
    /// after each command's state mutation completes — see
    /// [`ApplyHook`] for semantics.
    #[must_use]
    pub fn with_apply_hook(hook: Arc<dyn ApplyHook>) -> Self {
        Self {
            inner: Arc::new(futures::lock::Mutex::new(StateMachineInner::new())),
            apply_hook: Some(hook),
            metrics: None,
        }
    }

    /// Builder: attach the prometheus metrics struct so per-replica
    /// `apply` and `apply_hook_duration` tick.
    #[must_use]
    pub fn with_apply_metrics(mut self, metrics: Arc<super::ClusterControlMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Snapshot of the full state — for tests and read-side admin RPCs.
    #[allow(dead_code)] // exposed for future read-side admin RPCs
    pub async fn snapshot(&self) -> ControlSnapshot {
        let inner = self.inner.lock().await;
        ControlSnapshot {
            namespaces: inner.namespaces.clone(),
        }
    }

    /// Get a single namespace's shard map. `None` if the namespace
    /// has not been created via `CreateNamespace` yet.
    #[allow(dead_code)] // exposed for future read-side admin RPCs
    pub async fn namespace(&self, namespace_id: &str) -> Option<NamespaceShardMapSnapshot> {
        let inner = self.inner.lock().await;
        inner.namespaces.get(namespace_id).cloned()
    }
}

impl Default for ControlStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl RaftSnapshotBuilder<C> for ControlStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        let snap = ControlSnapshot {
            namespaces: inner.namespaces.clone(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let snapshot_id = format!(
            "snap-{}",
            inner
                .last_applied_log
                .as_ref()
                .map_or(0, openraft::LogId::index)
        );
        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log,
            last_membership: inner.last_membership.clone(),
            snapshot_id,
        };
        Ok(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<C> for ControlStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied_log, inner.last_membership.clone()))
    }

    #[tracing::instrument(skip(self, entries), level = "debug")]
    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: futures::Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend,
    {
        // Collect (cmd, responder) pairs so the inner-state mutex
        // is dropped before firing the apply hook. Some hook impls
        // (e.g. `ShardStoreApplyHook`) call into `RaftShardStore`
        // which spawns its own runtime threads — keeping the inner
        // lock held across them would serialize all subsequent
        // applies behind the heaviest one. Empirically that's
        // ~50-200 ms per `create_shard`, enough to stall heartbeat
        // replication.
        let mut hook_dispatch: Vec<ControlCommand> = Vec::new();
        let namespaces_after: usize;
        {
            let mut inner = self.inner.lock().await;
            while let Some((entry, responder)) = entries.try_next().await? {
                inner.last_applied_log = Some(entry.log_id);
                let response = match &entry.payload {
                    EntryPayload::Blank => ControlResponse::Applied,
                    EntryPayload::Normal(cmd) => {
                        let r = inner.apply_command(cmd);
                        if let Some(m) = self.metrics.as_ref() {
                            m.record_apply(super::ClusterControlMetrics::op_label(cmd));
                        }
                        hook_dispatch.push(cmd.clone());
                        r
                    }
                    EntryPayload::Membership(mem) => {
                        inner.last_membership =
                            openraft::StoredMembership::new(Some(entry.log_id), mem.clone());
                        ControlResponse::Applied
                    }
                };
                if let Some(responder) = responder {
                    responder.send(response);
                }
            }
            namespaces_after = inner.namespaces.len();
        }
        // Update the namespace gauge once per apply batch; the
        // value is replicated to every node so the gauge is
        // identical across the cluster once consensus converges.
        if let Some(m) = self.metrics.as_ref() {
            m.namespaces
                .set(i64::try_from(namespaces_after).unwrap_or(i64::MAX));
        }
        // Apply hook fires AFTER state mutation. Each method on
        // `ApplyHook` is contracted to be safe against replay
        // (idempotent) and to do its own background spawning when
        // the work is slow — that contract is what keeps openraft's
        // apply task moving through subsequent entries promptly.
        if let Some(hook) = self.apply_hook.as_ref() {
            for cmd in &hook_dispatch {
                let op = super::ClusterControlMetrics::op_label(cmd);
                let started = std::time::Instant::now();
                let _span = tracing::debug_span!(
                    "cluster_control.apply_hook",
                    op,
                    cmd = %cmd,
                )
                .entered();
                super::store::OpenRaftControlStore::dispatch_hook(hook.as_ref(), cmd);
                if let Some(m) = self.metrics.as_ref() {
                    m.record_hook_duration(op, started.elapsed());
                }
            }
        }
        Ok(())
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<<C as openraft::RaftTypeConfig>::SnapshotData, io::Error> {
        Ok(Cursor::new(Vec::new()))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<C>,
        snapshot: <C as openraft::RaftTypeConfig>::SnapshotData,
    ) -> Result<(), io::Error> {
        let data = snapshot.into_inner();
        let snap: ControlSnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut inner = self.inner.lock().await;
        inner.namespaces = snap.namespaces;
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = ControlSnapshot {
            namespaces: inner.namespaces.clone(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: Some(*last),
            last_membership: inner.last_membership.clone(),
            snapshot_id: format!("snap-{}", last.index()),
        };
        Ok(Some(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use uuid::Uuid;

    fn ns() -> String {
        "ns-test".to_owned()
    }

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0x4242))
    }

    fn shard(byte: u8) -> ShardId {
        ShardId(Uuid::from_u128(u128::from(byte) << 120))
    }

    fn full_range(s: ShardId) -> ShardRecord {
        ShardRecord {
            shard_id: s,
            range_start: [0u8; 32],
            range_end: [0xFFu8; 32],
            leader_node: NodeId(1),
        }
    }

    #[test]
    fn create_namespace_inserts_initial_shards_and_bumps_version_to_1() {
        let mut sm = StateMachineInner::new();
        let resp = sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        assert_eq!(resp, ControlResponse::NamespaceCreated { shard_count: 1 });
        let snap = sm.namespaces.get(&ns()).expect("ns inserted");
        assert_eq!(snap.version, 1);
        assert_eq!(snap.shards.len(), 1);
        assert_eq!(snap.shards[0].shard_id, shard(1));
    }

    #[test]
    fn create_namespace_is_idempotent_on_replay() {
        let mut sm = StateMachineInner::new();
        let cmd = ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        };
        let _ = sm.apply_command(&cmd);
        let resp2 = sm.apply_command(&cmd);
        // Second apply must NOT bump version or duplicate shards —
        // openraft replays committed entries on snapshot install
        // followed by tail catch-up, and any non-idempotent apply
        // would diverge replicas.
        assert_eq!(resp2, ControlResponse::NamespaceCreated { shard_count: 1 });
        assert_eq!(sm.namespaces[&ns()].version, 1);
        assert_eq!(sm.namespaces[&ns()].shards.len(), 1);
    }

    #[test]
    fn record_split_replaces_source_with_two_halves() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let mut midpoint = [0u8; 32];
        midpoint[0] = 0x80;
        let resp = sm.apply_command(&ControlCommand::RecordSplit {
            namespace_id: ns(),
            source_shard_id: shard(1),
            new_shard_id: shard(2),
            midpoint,
            new_leader: NodeId(2),
        });
        assert_eq!(
            resp,
            ControlResponse::SplitRecorded {
                new_shard_id: shard(2)
            }
        );
        let snap = &sm.namespaces[&ns()];
        assert_eq!(snap.shards.len(), 2);
        assert_eq!(snap.version, 2);
        assert_eq!(snap.shards[0].shard_id, shard(1));
        assert_eq!(snap.shards[0].range_end, midpoint);
        assert_eq!(snap.shards[1].shard_id, shard(2));
        assert_eq!(snap.shards[1].range_start, midpoint);
        assert_eq!(snap.shards[1].range_end, [0xFFu8; 32]);
    }

    #[test]
    fn record_split_is_idempotent_when_new_shard_already_present() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let mut midpoint = [0u8; 32];
        midpoint[0] = 0x80;
        let cmd = ControlCommand::RecordSplit {
            namespace_id: ns(),
            source_shard_id: shard(1),
            new_shard_id: shard(2),
            midpoint,
            new_leader: NodeId(2),
        };
        sm.apply_command(&cmd);
        let v_after_first = sm.namespaces[&ns()].version;
        sm.apply_command(&cmd);
        assert_eq!(
            sm.namespaces[&ns()].version,
            v_after_first,
            "replay must not bump version a second time",
        );
        assert_eq!(sm.namespaces[&ns()].shards.len(), 2);
    }

    #[test]
    fn record_merge_marks_retired_and_extends_surviving_range() {
        let mut sm = StateMachineInner::new();
        let mut mid = [0u8; 32];
        mid[0] = 0x80;
        // Pre-split state: two shards covering [0x00…, 0xFF…)
        let s1 = ShardRecord {
            shard_id: shard(1),
            range_start: [0u8; 32],
            range_end: mid,
            leader_node: NodeId(1),
        };
        let s2 = ShardRecord {
            shard_id: shard(2),
            range_start: mid,
            range_end: [0xFFu8; 32],
            leader_node: NodeId(2),
        };
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![s1, s2],
        });
        let resp = sm.apply_command(&ControlCommand::RecordMerge {
            namespace_id: ns(),
            surviving_shard_id: shard(1),
            retired_shard_id: shard(2),
            new_range_start: [0u8; 32],
            new_range_end: [0xFFu8; 32],
        });
        assert_eq!(resp, ControlResponse::Applied);
        let snap = &sm.namespaces[&ns()];
        let surv = snap
            .shards
            .iter()
            .find(|s| s.shard_id == shard(1))
            .expect("surviving in map");
        assert_eq!(surv.range_end, [0xFFu8; 32], "surviving range expanded");
        let ret = snap
            .shards
            .iter()
            .find(|s| s.shard_id == shard(2))
            .expect("retired still in map until RetireShard");
        assert!(ret.is_retiring, "retired shard flagged");
    }

    #[test]
    fn retire_shard_finalizes_removal_after_merge() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1)), full_range(shard(2))],
        });
        let v_before = sm.namespaces[&ns()].version;
        sm.apply_command(&ControlCommand::RetireShard {
            namespace_id: ns(),
            shard_id: shard(2),
        });
        assert_eq!(sm.namespaces[&ns()].shards.len(), 1);
        assert!(sm.namespaces[&ns()].version > v_before);
        // Replay must not bump version again.
        let v_after_first = sm.namespaces[&ns()].version;
        sm.apply_command(&ControlCommand::RetireShard {
            namespace_id: ns(),
            shard_id: shard(2),
        });
        assert_eq!(sm.namespaces[&ns()].version, v_after_first);
    }

    #[test]
    fn snapshot_round_trips_through_install_path() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let snap = ControlSnapshot {
            namespaces: sm.namespaces.clone(),
        };
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let parsed: ControlSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(parsed.namespaces.len(), 1);
        assert_eq!(parsed.namespaces[&ns()].shards.len(), 1);
        assert_eq!(parsed.namespaces[&ns()].shards[0].shard_id, shard(1));
    }
}
