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
    /// Next pin ID.
    next_pin_id: u64,
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
            .advance_watermark(view_id, SequenceNumber(100))
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
        let result = store.advance_watermark(view_id, SequenceNumber(50));
        assert!(result.is_err());
    }

    #[test]
    fn mvcc_pin_lifecycle() {
        let mut store = ViewStore::new();
        let desc = test_descriptor();
        let view_id = store.create_view(desc).unwrap_or_else(|_| unreachable!());
        store
            .advance_watermark(view_id, SequenceNumber(100))
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
}
