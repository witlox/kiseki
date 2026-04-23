//! Audit log store with `AuditOps` trait.
//!
//! Per-tenant sharding (ADR-009): each tenant's events go to a
//! separate shard. System-wide events go to a system shard.
//!
//! Uses `Mutex` for interior mutability so that `AuditOps` methods
//! can take `&self` (required for Raft-backed implementations).

use std::collections::HashMap;
use std::sync::Mutex;

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

/// Audit log operations trait.
///
/// All methods take `&self` — implementations use interior mutability.
pub trait AuditOps {
    /// Append an audit event. Routes to the appropriate tenant or system shard.
    fn append(&self, event: AuditEvent);

    /// Query audit events for a tenant or system shard.
    fn query(&self, q: &AuditQuery) -> Vec<AuditEvent>;

    /// Get the tip (latest sequence) for a tenant or system shard.
    fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber;

    /// Total event count across all shards.
    fn total_events(&self) -> usize;

    /// Tenant-scoped export (I-A2).
    fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent>;
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
    shards: Mutex<HashMap<ShardKey, AuditShard>>,
}

impl AuditLog {
    /// Create an empty audit log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shards: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditOps for AuditLog {
    fn append(&self, mut event: AuditEvent) {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = event.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let shard = shards.entry(key).or_insert_with(|| AuditShard {
            events: Vec::new(),
            tip: SequenceNumber(0),
        });

        let next_seq = SequenceNumber(shard.tip.0 + 1);
        event.sequence = next_seq;
        shard.tip = next_seq;
        shard.events.push(event);
    }

    fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = q.tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let Some(shard) = shards.get(&key) else {
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
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        shards.get(&key).map_or(SequenceNumber(0), |s| s.tip)
    }

    fn total_events(&self) -> usize {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards.values().map(|s| s.events.len()).sum()
    }

    fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut events = Vec::new();

        if let Some(shard) = shards.get(&ShardKey::Tenant(tenant_id)) {
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
    fn append_and_query() {
        let log = AuditLog::new();
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
        let log = AuditLog::new();
        let tenant_a = OrgId(uuid::Uuid::from_u128(1));
        let tenant_b = OrgId(uuid::Uuid::from_u128(2));

        log.append(make_event(Some(tenant_a), AuditEventType::DataWrite));
        log.append(make_event(Some(tenant_b), AuditEventType::DataRead));
        log.append(make_event(None, AuditEventType::AdminAction));

        assert_eq!(log.tip(Some(tenant_a)), SequenceNumber(1));
        assert_eq!(log.tip(Some(tenant_b)), SequenceNumber(1));
        assert_eq!(log.tip(None), SequenceNumber(1));
        assert_eq!(log.total_events(), 3);
    }

    #[test]
    fn query_with_type_filter() {
        let log = AuditLog::new();
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
        let log = AuditLog::new();
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

    #[test]
    fn append_only_order_preserved_ia1() {
        let log = AuditLog::new();

        // Append 5 events and verify they come back in order with
        // monotonically increasing sequence numbers.
        for _ in 0..5 {
            log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        }

        let events = log.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        });
        assert_eq!(events.len(), 5);

        // Verify monotonic ordering.
        for i in 0..events.len() - 1 {
            assert!(
                events[i].sequence < events[i + 1].sequence,
                "sequence numbers must be strictly increasing"
            );
        }

        // I-A1: no mutation API — AuditOps has append/query/tip/export but
        // no update or delete methods. The trait itself enforces append-only.
    }

    #[test]
    fn event_count_matches_after_multiple_appends() {
        let log = AuditLog::new();
        assert_eq!(log.total_events(), 0);

        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        log.append(make_event(Some(test_tenant()), AuditEventType::DataRead));
        log.append(make_event(None, AuditEventType::AdminAction));

        assert_eq!(log.total_events(), 3);
    }

    #[test]
    fn empty_store_returns_empty_results() {
        let log = AuditLog::new();

        // Query returns empty.
        let events = log.query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        });
        assert!(events.is_empty());

        // Tip is zero.
        assert_eq!(log.tip(Some(test_tenant())), SequenceNumber(0));
        assert_eq!(log.tip(None), SequenceNumber(0));

        // Total is zero.
        assert_eq!(log.total_events(), 0);

        // Export returns empty.
        let export = log.tenant_export(test_tenant());
        assert!(export.is_empty());
    }

    #[test]
    fn tip_advances_with_appends() {
        let log = AuditLog::new();
        assert_eq!(log.tip(Some(test_tenant())), SequenceNumber(0));

        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        assert_eq!(log.tip(Some(test_tenant())), SequenceNumber(1));

        log.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        assert_eq!(log.tip(Some(test_tenant())), SequenceNumber(2));
    }
}
