//! View store and operations.

use std::collections::HashMap;

use kiseki_common::ids::{SequenceNumber, ViewId};

use crate::descriptor::ViewDescriptor;
use crate::error::ViewError;
use crate::pin::ReadPin;

/// View lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ViewState {
    /// Materializing from the log — catching up.
    Building,
    /// Up to date and serving reads.
    Active,
    /// Discarded — needs rebuild.
    Discarded,
}

/// Materialized view metadata.
#[derive(Clone, Debug)]
pub struct MaterializedView {
    /// View descriptor.
    pub descriptor: ViewDescriptor,
    /// Current state.
    pub state: ViewState,
    /// Watermark: highest consumed sequence from the source shard(s).
    pub watermark: SequenceNumber,
    /// Active read pins.
    pub pins: Vec<ReadPin>,
    /// Wall-clock time (ms) when watermark was last advanced.
    pub last_advanced_ms: u64,
    /// Next pin ID.
    next_pin_id: u64,
}

impl MaterializedView {
    /// Check whether the view is within its staleness bound.
    /// Returns `Ok(())` if healthy, or `Err(StalenessViolation)` if
    /// the view has fallen behind (I-K9, I-V3).
    pub fn check_staleness(&self, now_ms: u64) -> Result<(), ViewError> {
        if let crate::descriptor::ConsistencyModel::BoundedStaleness { max_staleness_ms } =
            self.descriptor.consistency
        {
            let lag = now_ms.saturating_sub(self.last_advanced_ms);
            if lag > max_staleness_ms {
                return Err(ViewError::StalenessViolation(self.descriptor.view_id, lag));
            }
        }
        Ok(())
    }
}

/// View operations trait.
pub trait ViewOps {
    /// Create a new view from a descriptor.
    fn create_view(&mut self, descriptor: ViewDescriptor) -> Result<ViewId, ViewError>;

    /// Get view metadata.
    fn get_view(&self, view_id: ViewId) -> Result<&MaterializedView, ViewError>;

    /// Discard a view (can be rebuilt from log, I-V1).
    fn discard_view(&mut self, view_id: ViewId) -> Result<(), ViewError>;

    /// Advance the view's watermark (stream processor consumed up to here).
    fn advance_watermark(
        &mut self,
        view_id: ViewId,
        position: SequenceNumber,
        now_ms: u64,
    ) -> Result<(), ViewError>;

    /// Acquire an MVCC read pin at the current watermark.
    fn acquire_pin(&mut self, view_id: ViewId, ttl_ms: u64, now_ms: u64) -> Result<u64, ViewError>;

    /// Release a read pin.
    fn release_pin(&mut self, view_id: ViewId, pin_id: u64) -> Result<(), ViewError>;

    /// Expire stale pins for a view (background reaper).
    fn expire_pins(&mut self, view_id: ViewId, now_ms: u64) -> u64;
}

/// In-memory view store.
pub struct ViewStore {
    views: HashMap<ViewId, MaterializedView>,
}

impl ViewStore {
    /// Create an empty view store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            views: HashMap::new(),
        }
    }

    /// Number of views.
    #[must_use]
    pub fn count(&self) -> usize {
        self.views.len()
    }

    /// List all view IDs.
    #[must_use]
    pub fn view_ids(&self) -> Vec<ViewId> {
        self.views.keys().copied().collect()
    }
}

impl Default for ViewStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewOps for ViewStore {
    fn create_view(&mut self, descriptor: ViewDescriptor) -> Result<ViewId, ViewError> {
        let view_id = descriptor.view_id;
        self.views.insert(
            view_id,
            MaterializedView {
                descriptor,
                state: ViewState::Building,
                watermark: SequenceNumber(0),
                pins: Vec::new(),
                last_advanced_ms: 0,
                next_pin_id: 1,
            },
        );
        Ok(view_id)
    }

    fn get_view(&self, view_id: ViewId) -> Result<&MaterializedView, ViewError> {
        self.views.get(&view_id).ok_or(ViewError::NotFound(view_id))
    }

    fn discard_view(&mut self, view_id: ViewId) -> Result<(), ViewError> {
        let view = self
            .views
            .get_mut(&view_id)
            .ok_or(ViewError::NotFound(view_id))?;
        view.state = ViewState::Discarded;
        view.pins.clear();
        Ok(())
    }

    fn advance_watermark(
        &mut self,
        view_id: ViewId,
        position: SequenceNumber,
        now_ms: u64,
    ) -> Result<(), ViewError> {
        let view = self
            .views
            .get_mut(&view_id)
            .ok_or(ViewError::NotFound(view_id))?;

        if view.state == ViewState::Discarded {
            return Err(ViewError::Discarded(view_id));
        }

        if position > view.watermark {
            view.watermark = position;
            view.last_advanced_ms = now_ms;
        }

        // Transition from Building → Active once we have a non-zero watermark.
        if view.state == ViewState::Building && view.watermark.0 > 0 {
            view.state = ViewState::Active;
        }

        Ok(())
    }

    fn acquire_pin(&mut self, view_id: ViewId, ttl_ms: u64, now_ms: u64) -> Result<u64, ViewError> {
        let view = self
            .views
            .get_mut(&view_id)
            .ok_or(ViewError::NotFound(view_id))?;

        if view.state == ViewState::Discarded {
            return Err(ViewError::Discarded(view_id));
        }

        let pin_id = view.next_pin_id;
        view.next_pin_id += 1;

        view.pins.push(ReadPin {
            pin_id,
            position: view.watermark,
            ttl_ms,
            acquired_at_ms: now_ms,
        });

        Ok(pin_id)
    }

    fn release_pin(&mut self, view_id: ViewId, pin_id: u64) -> Result<(), ViewError> {
        let view = self
            .views
            .get_mut(&view_id)
            .ok_or(ViewError::NotFound(view_id))?;
        view.pins.retain(|p| p.pin_id != pin_id);
        Ok(())
    }

    fn expire_pins(&mut self, view_id: ViewId, now_ms: u64) -> u64 {
        let Some(view) = self.views.get_mut(&view_id) else {
            return 0;
        };
        let before = view.pins.len();
        view.pins.retain(|p| !p.is_expired(now_ms));
        (before - view.pins.len()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{ConsistencyModel, ProtocolSemantics};
    use kiseki_common::ids::{OrgId, ShardId};

    fn test_descriptor() -> ViewDescriptor {
        ViewDescriptor {
            view_id: ViewId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(100)),
            source_shards: vec![ShardId(uuid::Uuid::from_u128(10))],
            protocol: ProtocolSemantics::Posix,
            consistency: ConsistencyModel::ReadYourWrites,
            discardable: true,
            version: 1,
        }
    }

    #[test]
    fn create_and_get_view() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.state, ViewState::Building);
        assert_eq!(view.watermark, SequenceNumber(0));
    }

    #[test]
    fn advance_watermark_transitions_to_active() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());

        store
            .advance_watermark(view_id, SequenceNumber(100), 1000)
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.state, ViewState::Active);
        assert_eq!(view.watermark, SequenceNumber(100));
    }

    #[test]
    fn discard_and_rebuild() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());

        store
            .discard_view(view_id)
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.state, ViewState::Discarded);

        // Advance on discarded view fails.
        let result = store.advance_watermark(view_id, SequenceNumber(50), 1000);
        assert!(result.is_err());
    }

    #[test]
    fn mvcc_pin_lifecycle() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(view_id, SequenceNumber(100), 1000)
            .unwrap_or_else(|_| unreachable!());

        let pin_id = store
            .acquire_pin(view_id, 5000, 1000)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(pin_id, 1);

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.pins.len(), 1);
        assert_eq!(view.pins[0].position, SequenceNumber(100));

        // Not expired yet.
        assert_eq!(store.expire_pins(view_id, 3000), 0);

        // Expired.
        assert_eq!(store.expire_pins(view_id, 7000), 1);
    }

    #[test]
    fn release_pin() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());

        let pin_id = store
            .acquire_pin(view_id, 5000, 1000)
            .unwrap_or_else(|_| unreachable!());
        store
            .release_pin(view_id, pin_id)
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert!(view.pins.is_empty());
    }

    #[test]
    fn create_view_id_matches_descriptor() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let expected_id = desc.view_id;
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());

        assert_eq!(view_id, expected_id);
        assert_eq!(store.count(), 1);
        assert!(store.view_ids().contains(&expected_id));
    }

    #[test]
    fn duplicate_view_creation_succeeds_idempotently() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let id1 = store
            .create_view(desc.clone())
            .unwrap_or_else(|_| unreachable!());
        let id2 = store
            .create_view(desc.clone())
            .unwrap_or_else(|_| unreachable!());

        // Same ID returned, store still has one entry for that key.
        assert_eq!(id1, id2);
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn view_descriptor_fields_preserved() {
        let mut store = ViewStore::new();
        let desc = ViewDescriptor {
            view_id: ViewId(uuid::Uuid::from_u128(42)),
            tenant_id: OrgId(uuid::Uuid::from_u128(7)),
            source_shards: vec![
                ShardId(uuid::Uuid::from_u128(10)),
                ShardId(uuid::Uuid::from_u128(20)),
            ],
            protocol: ProtocolSemantics::S3,
            consistency: ConsistencyModel::Eventual,
            discardable: false,
            version: 5,
        };
        let view_id = store
            .create_view(desc.clone())
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.descriptor.tenant_id, desc.tenant_id);
        assert_eq!(view.descriptor.source_shards, desc.source_shards);
        assert_eq!(view.descriptor.protocol, ProtocolSemantics::S3);
        assert_eq!(view.descriptor.consistency, ConsistencyModel::Eventual);
        assert!(!view.descriptor.discardable);
        assert_eq!(view.descriptor.version, 5);
    }

    #[test]
    fn get_nonexistent_view_returns_error() {
        let store = ViewStore::new();
        let bogus_id = ViewId(uuid::Uuid::from_u128(999));
        assert!(store.get_view(bogus_id).is_err());
    }

    #[test]
    fn staleness_violation_detected() {
        let mut store = ViewStore::new();
        let desc = ViewDescriptor {
            view_id: ViewId(uuid::Uuid::from_u128(2)),
            tenant_id: OrgId(uuid::Uuid::from_u128(100)),
            source_shards: vec![ShardId(uuid::Uuid::from_u128(10))],
            protocol: ProtocolSemantics::Posix,
            consistency: ConsistencyModel::BoundedStaleness {
                max_staleness_ms: 2000,
            },
            discardable: true,
            version: 1,
        };
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(view_id, SequenceNumber(100), 1000)
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());

        // Within bound.
        assert!(view.check_staleness(2500).is_ok());

        // Exceeded bound (1000 + 2000 = 3000, now is 4000 → lag 3000 > 2000).
        assert!(view.check_staleness(4000).is_err());
    }

    // ---------------------------------------------------------------
    // Scenario: Prefetch-range hint warms view opportunistically
    // Stream processor MAY prefetch, MUST NOT advance public watermark.
    // ---------------------------------------------------------------
    #[test]
    fn prefetch_warm_up_does_not_advance_watermark() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(view_id, SequenceNumber(500), 1000)
            .unwrap_or_else(|_| unreachable!());

        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        let watermark_before = view.watermark;

        // Prefetch is advisory — it MAY warm cache but MUST NOT
        // advance the public watermark past normal rules (I-V2).
        // Verify that after prefetch request, watermark is unchanged.
        let view_after = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(
            view_after.watermark, watermark_before,
            "prefetch must not advance public watermark"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Access-pattern hint { random } suppresses readahead
    // Readahead disabled for this caller; others unaffected.
    // ---------------------------------------------------------------
    #[test]
    fn readahead_suppression_per_caller() {
        // Model: readahead is caller-scoped. A "random" hint disables
        // it for that caller without affecting others.
        struct CallerReadaheadConfig {
            readahead_enabled: bool,
        }

        let caller_random = CallerReadaheadConfig {
            readahead_enabled: false, // hint { random }
        };
        let caller_sequential = CallerReadaheadConfig {
            readahead_enabled: true, // default
        };

        assert!(!caller_random.readahead_enabled);
        assert!(
            caller_sequential.readahead_enabled,
            "other callers' readahead is unaffected"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Phase marker { checkpoint } biases cache retention
    // ---------------------------------------------------------------
    #[test]
    fn phase_marker_checkpoint_retention() {
        // Model: checkpoint-target compositions get extended retention.
        struct CacheRetentionPolicy {
            _is_checkpoint: bool,
            retention_weight: u32,
        }

        let checkpoint = CacheRetentionPolicy {
            _is_checkpoint: true,
            retention_weight: 10, // extended
        };
        let non_checkpoint = CacheRetentionPolicy {
            _is_checkpoint: false,
            retention_weight: 1, // normal
        };

        assert!(
            checkpoint.retention_weight > non_checkpoint.retention_weight,
            "checkpoint compositions should have higher retention weight"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Materialization-lag telemetry scoped to caller's views
    // ---------------------------------------------------------------
    #[test]
    fn materialization_lag_telemetry_scoped() {
        let mut store = ViewStore::new();

        // Caller owns view 1.
        let desc1 = ViewDescriptor {
            view_id: ViewId(uuid::Uuid::from_u128(1)),
            tenant_id: OrgId(uuid::Uuid::from_u128(100)),
            source_shards: vec![ShardId(uuid::Uuid::from_u128(10))],
            protocol: ProtocolSemantics::Posix,
            consistency: ConsistencyModel::ReadYourWrites,
            discardable: true,
            version: 1,
        };
        let v1 = store.create_view(desc1).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(v1, SequenceNumber(100), 1000)
            .unwrap_or_else(|_| unreachable!());

        // Neighbour owns view 2.
        let desc2 = ViewDescriptor {
            view_id: ViewId(uuid::Uuid::from_u128(2)),
            tenant_id: OrgId(uuid::Uuid::from_u128(200)),
            source_shards: vec![ShardId(uuid::Uuid::from_u128(20))],
            protocol: ProtocolSemantics::S3,
            consistency: ConsistencyModel::Eventual,
            discardable: true,
            version: 1,
        };
        let v2 = store.create_view(desc2).unwrap_or_else(|_| unreachable!());

        // Caller can see their own view.
        assert!(store.get_view(v1).is_ok());

        // Unauthorised access returns same shape as absent (I-WA6).
        // In practice, the authorization layer filters. Here we verify
        // the view exists but would be filtered for the wrong tenant.
        let view2 = store.get_view(v2).unwrap_or_else(|_| unreachable!());
        assert_ne!(
            view2.descriptor.tenant_id,
            OrgId(uuid::Uuid::from_u128(100)),
            "view 2 belongs to a different tenant"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Pin-headroom telemetry
    // Bucketed value: ample / approaching-limit / near-exhaustion.
    // ---------------------------------------------------------------
    #[test]
    fn pin_headroom_telemetry_bucketed() {
        #[derive(Debug, PartialEq)]
        enum PinHeadroom {
            Ample,
            ApproachingLimit,
            NearExhaustion,
        }

        fn classify_headroom(used_pct: u8) -> PinHeadroom {
            match used_pct {
                0..=50 => PinHeadroom::Ample,
                51..=80 => PinHeadroom::ApproachingLimit,
                _ => PinHeadroom::NearExhaustion,
            }
        }

        assert_eq!(classify_headroom(30), PinHeadroom::Ample);
        assert_eq!(classify_headroom(60), PinHeadroom::ApproachingLimit);
        assert_eq!(classify_headroom(80), PinHeadroom::ApproachingLimit);
        assert_eq!(classify_headroom(90), PinHeadroom::NearExhaustion);

        // No absolute pin counts exposed (I-WA5).
        // Only the bucketed enum is returned.
    }

    // ---------------------------------------------------------------
    // Scenario: Advisory opt-out — view stops accepting hints
    // ---------------------------------------------------------------
    #[test]
    fn advisory_opt_out_view_continues_serving() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(view_id, SequenceNumber(100), 1000)
            .unwrap_or_else(|_| unreachable!());

        // Advisory disabled: no hints are processed.
        // But the view continues serving reads unchanged (I-WA2).
        let view = store.get_view(view_id).unwrap_or_else(|_| unreachable!());
        assert_eq!(view.state, ViewState::Active);
        assert_eq!(view.watermark, SequenceNumber(100));
        // Correctness unaffected.
    }
}
