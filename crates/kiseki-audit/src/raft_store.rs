//! Raft-ready audit store — append-only command log state machine.
//!
//! Per-tenant audit shards as separate command logs. The state machine
//! is strictly append-only — no command can mutate or delete an
//! existing event (I-A1).

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{OrgId, SequenceNumber};
use serde::{Deserialize, Serialize};

use crate::event::{AuditEvent, AuditEventType};
use crate::store::{AuditOps, AuditQuery};
use kiseki_common::locks::LockOrDie;

/// Audit commands — append-only by construction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuditCommand {
    /// Append an event to a tenant or system shard.
    AppendEvent {
        /// Tenant ID (None = system shard).
        tenant_id: Option<[u8; 16]>,
        /// Event type name.
        event_type: String,
        /// Actor.
        actor: String,
        /// Description.
        description: String,
    },
}

impl std::fmt::Display for AuditCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppendEvent { event_type, .. } => write!(f, "AppendEvent({event_type})"),
        }
    }
}

/// Shard key.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum ShardKey {
    Tenant(OrgId),
    System,
}

/// Per-shard state.
struct AuditShard {
    events: Vec<AuditEvent>,
    tip: SequenceNumber,
}

/// Inner state.
struct Inner {
    shards: HashMap<ShardKey, AuditShard>,
    log: Vec<(u64, AuditCommand)>,
    last_applied: u64,
}

/// Raft-ready audit log with command log.
pub struct RaftAuditStore {
    inner: Mutex<Inner>,
}

impl RaftAuditStore {
    /// Create an empty audit store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                shards: HashMap::new(),
                log: Vec::new(),
                last_applied: 0,
            }),
        }
    }

    /// Get the command log length.
    #[must_use]
    pub fn log_length(&self) -> usize {
        self.inner.lock().lock_or_die("raft_store.inner").log.len()
    }

    /// Get a snapshot of the current command log.
    #[must_use]
    pub fn command_log(&self) -> Vec<(u64, AuditCommand)> {
        self.inner
            .lock()
            .lock_or_die("raft_store.inner")
            .log
            .clone()
    }

    /// Apply a command externally (for persistent store wrapper).
    /// Returns the log index assigned to this command.
    pub fn apply_command_external(&self, event: AuditEvent) -> u64 {
        // Delegates to the AuditOps::append impl, then returns index.
        self.append(event);
        self.log_length() as u64
    }

    /// Reconstruct a `RaftAuditStore` from a persisted command log.
    pub fn from_commands(commands: impl Iterator<Item = (u64, AuditCommand)>) -> Self {
        let store = Self::new();
        {
            let mut inner = store.inner.lock().lock_or_die("raft_store.inner");
            for (idx, cmd) in commands {
                inner.log.push((idx, cmd));
            }
        }
        store.replay();
        store
    }

    /// Replay the command log to rebuild state (e.g., after snapshot restore).
    pub fn replay(&self) {
        let mut inner = self.inner.lock().lock_or_die("raft_store.inner");

        let log = inner.log.clone();
        inner.shards.clear();
        inner.last_applied = 0;

        for (index, cmd) in &log {
            if *index <= inner.last_applied {
                continue;
            }
            inner.last_applied = *index;

            let AuditCommand::AppendEvent {
                tenant_id,
                event_type,
                actor,
                description,
            } = cmd;
            {
                let org_id = tenant_id.map(|b| OrgId(uuid::Uuid::from_bytes(b)));
                let key = org_id.map_or(ShardKey::System, ShardKey::Tenant);
                let shard = inner.shards.entry(key).or_insert_with(|| AuditShard {
                    events: Vec::new(),
                    tip: SequenceNumber(0),
                });

                let next_seq = SequenceNumber(shard.tip.0 + 1);
                let timestamp = kiseki_common::time::DeltaTimestamp {
                    hlc: kiseki_common::time::HybridLogicalClock {
                        physical_ms: *index,
                        logical: 0,
                        node_id: kiseki_common::ids::NodeId(0),
                    },
                    wall: kiseki_common::time::WallTime {
                        millis_since_epoch: *index,
                        timezone: "UTC".into(),
                    },
                    quality: kiseki_common::time::ClockQuality::Ntp,
                };

                shard.events.push(AuditEvent {
                    sequence: next_seq,
                    timestamp,
                    event_type: Self::event_type_from_str(event_type),
                    tenant_id: org_id,
                    actor: actor.clone(),
                    description: description.clone(),
                });
                shard.tip = next_seq;
            }
        }
    }

    fn event_type_from_str(s: &str) -> AuditEventType {
        match s {
            "KeyGeneration" => AuditEventType::KeyGeneration,
            "KeyRotation" => AuditEventType::KeyRotation,
            "KeyDestruction" => AuditEventType::KeyDestruction,
            "KeyAccess" => AuditEventType::KeyAccess,
            "ReEncryption" => AuditEventType::ReEncryption,
            "DataRead" => AuditEventType::DataRead,
            "DataWrite" => AuditEventType::DataWrite,
            "DataDelete" => AuditEventType::DataDelete,
            "AuthSuccess" => AuditEventType::AuthSuccess,
            "AuthFailure" => AuditEventType::AuthFailure,
            "TenantLifecycle" => AuditEventType::TenantLifecycle,
            "PolicyChange" => AuditEventType::PolicyChange,
            "MaintenanceMode" => AuditEventType::MaintenanceMode,
            "AdvisoryWorkflow" => AuditEventType::AdvisoryWorkflow,
            "AdvisoryHint" => AuditEventType::AdvisoryHint,
            "AdvisoryBudgetExceeded" => AuditEventType::AdvisoryBudgetExceeded,
            _ => AuditEventType::AdminAction,
        }
    }

    /// Convert an `AuditEventType` to its string representation.
    #[must_use]
    pub fn event_type_to_str_pub(t: &AuditEventType) -> &'static str {
        Self::event_type_to_str(t)
    }

    fn event_type_to_str(t: &AuditEventType) -> &'static str {
        match t {
            AuditEventType::KeyGeneration => "KeyGeneration",
            AuditEventType::KeyRotation => "KeyRotation",
            AuditEventType::KeyDestruction => "KeyDestruction",
            AuditEventType::KeyAccess => "KeyAccess",
            AuditEventType::ReEncryption => "ReEncryption",
            AuditEventType::DataRead => "DataRead",
            AuditEventType::DataWrite => "DataWrite",
            AuditEventType::DataDelete => "DataDelete",
            AuditEventType::AuthSuccess => "AuthSuccess",
            AuditEventType::AuthFailure => "AuthFailure",
            AuditEventType::TenantLifecycle => "TenantLifecycle",
            AuditEventType::AdminAction => "AdminAction",
            AuditEventType::PolicyChange => "PolicyChange",
            AuditEventType::MaintenanceMode => "MaintenanceMode",
            AuditEventType::AdvisoryWorkflow => "AdvisoryWorkflow",
            AuditEventType::AdvisoryHint => "AdvisoryHint",
            AuditEventType::AdvisoryBudgetExceeded => "AdvisoryBudgetExceeded",
            AuditEventType::SecurityDowngradeEnabled => "SecurityDowngradeEnabled",
        }
    }
}

impl Default for RaftAuditStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditOps for RaftAuditStore {
    fn append(&self, event: AuditEvent) {
        let mut inner = self.inner.lock().lock_or_die("raft_store.inner");

        let cmd = AuditCommand::AppendEvent {
            tenant_id: event.tenant_id.map(|o| *o.0.as_bytes()),
            event_type: Self::event_type_to_str(&event.event_type).to_owned(),
            actor: event.actor.clone(),
            description: event.description.clone(),
        };

        let index = inner.log.len() as u64 + 1;
        inner.log.push((index, cmd));
        inner.last_applied = index;

        // Apply to state.
        let key = event.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let shard = inner.shards.entry(key).or_insert_with(|| AuditShard {
            events: Vec::new(),
            tip: SequenceNumber(0),
        });

        let next_seq = SequenceNumber(shard.tip.0 + 1);
        let mut stored_event = event;
        stored_event.sequence = next_seq;
        shard.tip = next_seq;
        shard.events.push(stored_event);
    }

    fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        let inner = self.inner.lock().lock_or_die("raft_store.inner");
        let key = q.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let Some(shard) = inner.shards.get(&key) else {
            return Vec::new();
        };

        shard
            .events
            .iter()
            .filter(|e| e.sequence >= q.from)
            .filter(|e| q.event_type.as_ref().is_none_or(|t| &e.event_type == t))
            .take(q.limit)
            .cloned()
            .collect()
    }

    fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber {
        let inner = self.inner.lock().lock_or_die("raft_store.inner");
        let key = tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        inner.shards.get(&key).map_or(SequenceNumber(0), |s| s.tip)
    }

    fn total_events(&self) -> usize {
        let inner = self.inner.lock().lock_or_die("raft_store.inner");
        inner.shards.values().map(|s| s.events.len()).sum()
    }

    fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent> {
        let inner = self.inner.lock().lock_or_die("raft_store.inner");
        let mut events = Vec::new();
        if let Some(shard) = inner.shards.get(&ShardKey::Tenant(tenant_id)) {
            events.extend(shard.events.iter().cloned());
        }
        events.sort_by_key(|e| e.sequence);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::NodeId;
    use kiseki_common::time::*;

    fn test_timestamp() -> DeltaTimestamp {
        DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: NodeId(1),
            },
            wall: WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: ClockQuality::Ntp,
        }
    }

    fn make_event(tenant: Option<OrgId>, event_type: AuditEventType) -> AuditEvent {
        AuditEvent {
            sequence: SequenceNumber(0),
            timestamp: test_timestamp(),
            event_type,
            tenant_id: tenant,
            actor: "test".into(),
            description: "test event".into(),
        }
    }

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    #[test]
    fn append_via_command_log() {
        let store = RaftAuditStore::new();
        store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));

        assert_eq!(store.log_length(), 1);
        assert_eq!(store.tip(Some(test_tenant())), SequenceNumber(1));
    }

    #[test]
    fn per_tenant_isolation() {
        let store = RaftAuditStore::new();
        let t1 = OrgId(uuid::Uuid::from_u128(1));
        let t2 = OrgId(uuid::Uuid::from_u128(2));

        store.append(make_event(Some(t1), AuditEventType::DataWrite));
        store.append(make_event(Some(t2), AuditEventType::DataRead));
        store.append(make_event(None, AuditEventType::AdminAction));

        assert_eq!(store.tip(Some(t1)), SequenceNumber(1));
        assert_eq!(store.tip(Some(t2)), SequenceNumber(1));
        assert_eq!(store.tip(None), SequenceNumber(1));
        assert_eq!(store.total_events(), 3);
    }

    #[test]
    fn append_only_no_mutation() {
        let store = RaftAuditStore::new();
        store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        store.append(make_event(Some(test_tenant()), AuditEventType::DataRead));

        // Both events exist — no mutation possible.
        let events = store.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, SequenceNumber(1));
        assert_eq!(events[1].sequence, SequenceNumber(2));
    }

    #[test]
    fn replay_rebuilds_state() {
        let store = RaftAuditStore::new();
        store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        store.append(make_event(Some(test_tenant()), AuditEventType::KeyRotation));
        store.append(make_event(None, AuditEventType::AdminAction));

        // Replay from log.
        store.replay();

        // State should be identical after replay.
        assert_eq!(store.tip(Some(test_tenant())), SequenceNumber(2));
        assert_eq!(store.tip(None), SequenceNumber(1));
        assert_eq!(store.total_events(), 3);

        let events = store.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        });
        assert_eq!(events.len(), 2);
    }
}
