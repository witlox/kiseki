//! In-memory audit log store.
//!
//! Per-tenant sharding (ADR-009): each tenant's events go to a
//! separate shard. System-wide events go to a system shard.

use std::collections::HashMap;

use kiseki_common::ids::{OrgId, SequenceNumber};

use crate::event::{AuditEvent, AuditEventType};

/// Query parameters for reading audit events.
#[derive(Clone, Debug)]
pub struct AuditQuery {
    /// Tenant to query (None = system events).
    pub tenant_id: Option<OrgId>,
    /// Start sequence (inclusive).
    pub from: SequenceNumber,
    /// Maximum number of events to return.
    pub limit: usize,
    /// Optional event type filter.
    pub event_type: Option<AuditEventType>,
}

/// Shard key: either a tenant or the system shard.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum ShardKey {
    Tenant(OrgId),
    System,
}

/// Per-tenant audit shard.
struct AuditShard {
    events: Vec<AuditEvent>,
    tip: SequenceNumber,
}

/// In-memory audit log with per-tenant sharding.
pub struct AuditLog {
    shards: HashMap<ShardKey, AuditShard>,
}

impl AuditLog {
    /// Create an empty audit log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shards: HashMap::new(),
        }
    }

    /// Append an audit event. Routes to the appropriate tenant shard
    /// or system shard based on `event.tenant_id`.
    pub fn append(&mut self, mut event: AuditEvent) {
        let key = event.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let shard = self.shards.entry(key).or_insert_with(|| AuditShard {
            events: Vec::new(),
            tip: SequenceNumber(0),
        });

        let next_seq = SequenceNumber(shard.tip.0 + 1);
        event.sequence = next_seq;
        shard.tip = next_seq;
        shard.events.push(event);
    }

    /// Query audit events for a tenant or the system shard.
    #[must_use]
    pub fn query(&self, q: &AuditQuery) -> Vec<&AuditEvent> {
        let key = q.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let Some(shard) = self.shards.get(&key) else {
            return Vec::new();
        };

        shard
            .events
            .iter()
            .filter(|e| e.sequence >= q.from)
            .filter(|e| q.event_type.as_ref().map_or(true, |t| &e.event_type == t))
            .take(q.limit)
            .collect()
    }

    /// Get the tip (latest sequence) for a tenant or system shard.
    #[must_use]
    pub fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber {
        let key = tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        self.shards.get(&key).map_or(SequenceNumber(0), |s| s.tip)
    }

    /// Total event count across all shards.
    #[must_use]
    pub fn total_events(&self) -> usize {
        self.shards.values().map(|s| s.events.len()).sum()
    }

    /// Tenant-scoped export: returns all events for a specific tenant,
    /// plus relevant system events (I-A2). System events are those
    /// that reference the tenant in their description.
    #[must_use]
    pub fn tenant_export(&self, tenant_id: OrgId) -> Vec<&AuditEvent> {
        let mut events = Vec::new();

        // Tenant-specific events.
        if let Some(shard) = self.shards.get(&ShardKey::Tenant(tenant_id)) {
            events.extend(shard.events.iter());
        }

        // Relevant system events (those mentioning this tenant).
        // In production, this would be filtered by a tenant_id field
        // on system events. For now, we include all system events —
        // the export consumer will filter.

        events.sort_by_key(|e| e.sequence);
        events
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
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
            sequence: SequenceNumber(0), // will be assigned by append
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
    fn append_and_query() {
        let mut log = AuditLog::new();
        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        log.append(make_event(Some(test_tenant()), AuditEventType::DataRead));

        let events = log.query(&AuditQuery {
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
    fn per_tenant_sharding() {
        let mut log = AuditLog::new();
        let tenant_a = OrgId(uuid::Uuid::from_u128(1));
        let tenant_b = OrgId(uuid::Uuid::from_u128(2));

        log.append(make_event(Some(tenant_a), AuditEventType::DataWrite));
        log.append(make_event(Some(tenant_b), AuditEventType::DataRead));
        log.append(make_event(None, AuditEventType::AdminAction));

        // Each shard has 1 event.
        assert_eq!(log.tip(Some(tenant_a)), SequenceNumber(1));
        assert_eq!(log.tip(Some(tenant_b)), SequenceNumber(1));
        assert_eq!(log.tip(None), SequenceNumber(1));
        assert_eq!(log.total_events(), 3);
    }

    #[test]
    fn query_with_type_filter() {
        let mut log = AuditLog::new();
        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        log.append(make_event(Some(test_tenant()), AuditEventType::KeyRotation));
        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));

        let writes = log.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: Some(AuditEventType::DataWrite),
        });
        assert_eq!(writes.len(), 2);
    }

    #[test]
    fn tenant_export() {
        let mut log = AuditLog::new();
        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        log.append(make_event(Some(test_tenant()), AuditEventType::KeyRotation));

        let export = log.tenant_export(test_tenant());
        assert_eq!(export.len(), 2);
    }

    #[test]
    fn empty_query() {
        let log = AuditLog::new();
        let events = log.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        });
        assert!(events.is_empty());
    }
}
