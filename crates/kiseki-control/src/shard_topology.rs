//! Shard topology — namespace-to-shard mapping and initial placement.
//!
//! ADR-033: initial shard topology, ratio-floor splits, persistent
//! namespace shard map. Gateway routing via `route_to_shard()`.
//!
//! Key types: `ShardRange`, `NamespaceShardMap`, `ShardTopologyConfig`.

use std::collections::HashMap;
use std::sync::RwLock;

use kiseki_common::ids::{NodeId, OrgId, ShardId};

use crate::error::ControlError;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Cluster-wide shard topology parameters (ADR-033 §1).
#[derive(Clone, Debug)]
pub struct ShardTopologyConfig {
    /// Multiplier applied to active node count. Default: 3.
    pub multiplier: u32,
    /// Maximum shards per namespace. Default: 64.
    pub shard_cap: u32,
    /// Minimum shards per namespace. Default: 3.
    pub shard_floor: u32,
    /// Minimum shards-per-node ratio before auto-split fires. Default: 1.5.
    pub ratio_floor: f64,
}

impl Default for ShardTopologyConfig {
    fn default() -> Self {
        Self {
            multiplier: 3,
            shard_cap: 64,
            shard_floor: 3,
            ratio_floor: 1.5,
        }
    }
}

/// Per-tenant shard bounds set by cluster admin (ADR-033 §1).
#[derive(Clone, Debug)]
pub struct TenantShardBounds {
    pub min_shards: u32,
    pub max_shards: u32,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Namespace lifecycle state during creation (ADV-033-1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceCreationState {
    /// Raft groups being formed.
    Creating,
    /// All shards healthy, map committed.
    Active,
}

/// A single shard's key range within a namespace (ADR-033 §4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShardRange {
    pub shard_id: ShardId,
    /// Inclusive lower bound of the 256-bit key range.
    pub range_start: [u8; 32],
    /// Exclusive upper bound of the 256-bit key range.
    pub range_end: [u8; 32],
    /// Best-effort leader placement (may be stale).
    pub leader_node: NodeId,
}

/// Persistent namespace-to-shard mapping (ADR-033 §4).
#[derive(Clone, Debug)]
pub struct NamespaceShardMap {
    pub namespace_id: String,
    pub tenant_id: OrgId,
    /// Monotonically increasing version on every mutation.
    pub version: u64,
    /// Sorted by `range_start` — disjoint, covering full key space.
    pub shards: Vec<ShardRange>,
    /// Creation state (ADV-033-1).
    pub state: NamespaceCreationState,
}

// ---------------------------------------------------------------------------
// Topology computation
// ---------------------------------------------------------------------------

/// Compute the initial shard count for a new namespace (ADR-033 §1).
///
/// ```text
/// initial_shards = max(min(multiplier × node_count, shard_cap), shard_floor)
/// ```
#[must_use]
pub fn compute_initial_shards(config: &ShardTopologyConfig, active_node_count: u32) -> u32 {
    let raw = config.multiplier.saturating_mul(active_node_count);
    let capped = raw.min(config.shard_cap);
    capped.max(config.shard_floor)
}

/// Compute disjoint shard ranges covering the full 256-bit key space.
///
/// Uniform subdivision: each shard gets `(2^256 - 1) / shard_count` width.
/// The last shard absorbs any remainder (its `range_end` is `[0xFF; 32]`).
#[must_use]
pub fn compute_shard_ranges(shard_count: u32, nodes: &[NodeId]) -> Vec<ShardRange> {
    assert!(shard_count > 0, "shard_count must be > 0");

    // We work with 256-bit integers represented as [u8; 32] big-endian.
    // For uniform subdivision, compute step = (2^256 - 1) / shard_count.
    // Simplified: we divide the 256-bit space uniformly.

    let mut ranges = Vec::with_capacity(shard_count as usize);

    for i in 0..shard_count {
        let range_start = range_point(i, shard_count);
        let range_end = if i == shard_count - 1 {
            [0xFF; 32]
        } else {
            range_point(i + 1, shard_count)
        };

        // Round-robin leader placement across available nodes.
        let leader = if nodes.is_empty() {
            NodeId(1)
        } else {
            nodes[i as usize % nodes.len()]
        };

        ranges.push(ShardRange {
            shard_id: ShardId(uuid::Uuid::new_v4()),
            range_start,
            range_end,
            leader_node: leader,
        });
    }

    ranges
}

/// Compute the i-th subdivision point of the 256-bit key space.
///
/// Returns `(i * (2^256 / shard_count))` as `[u8; 32]` big-endian.
fn range_point(i: u32, shard_count: u32) -> [u8; 32] {
    if i == 0 {
        return [0u8; 32];
    }

    // We need to compute (i * 2^256) / shard_count in 256-bit arithmetic.
    // Since we don't have a bigint library, we'll use a simpler approach:
    // divide byte-by-byte with carry, computing (i / shard_count) * 2^256.

    // Start with numerator = i, shift left 256 bits, divide by shard_count.
    // This is equivalent to long division of (i << 256) / shard_count.
    let mut result = [0u8; 32];
    let mut remainder: u64 = i as u64;

    for byte in &mut result {
        remainder <<= 8;
        *byte = (remainder / shard_count as u64) as u8;
        remainder %= shard_count as u64;
    }

    result
}

/// Route a hashed key to the correct shard (ADR-033 §5).
///
/// Binary search over sorted `ShardRange` list. O(log N) where N ≤ 64.
pub fn route_to_shard(map: &NamespaceShardMap, hashed_key: &[u8; 32]) -> Option<ShardId> {
    // Shards are sorted by range_start. Find the last shard whose
    // range_start <= hashed_key.
    let idx = map
        .shards
        .partition_point(|s| s.range_start.as_slice() <= hashed_key.as_slice());

    if idx == 0 {
        return None;
    }

    let shard = &map.shards[idx - 1];
    // Verify key is within [range_start, range_end).
    // The last shard's range_end is [0xFF; 32] and is inclusive (absorbs remainder).
    let is_last = idx == map.shards.len();
    if !is_last && hashed_key.as_slice() >= shard.range_end.as_slice() {
        return None;
    }

    Some(shard.shard_id)
}

/// Check whether the ratio floor is violated for a namespace and compute
/// how many shards are needed (ADR-033 §3).
///
/// Returns `Some(target_shards)` if splits are needed, `None` if ratio is satisfied.
#[must_use]
pub fn check_ratio_floor(
    config: &ShardTopologyConfig,
    current_shard_count: u32,
    active_node_count: u32,
) -> Option<u32> {
    if active_node_count == 0 {
        return None;
    }

    let ratio = current_shard_count as f64 / active_node_count as f64;
    if ratio >= config.ratio_floor {
        return None;
    }

    let target = (config.ratio_floor * active_node_count as f64).ceil() as u32;
    let capped = target.min(config.shard_cap);

    if capped > current_shard_count {
        Some(capped)
    } else {
        None
    }
}

/// Maximum concurrent ratio-floor splits allowed (ADR-033 §3, ADV-033-7).
#[must_use]
pub fn max_concurrent_splits(active_node_count: u32) -> u32 {
    (active_node_count / 5).max(1)
}

// ---------------------------------------------------------------------------
// Namespace shard map store
// ---------------------------------------------------------------------------

/// In-memory store for namespace shard maps.
///
/// In production this would be backed by the control plane Raft group (I-L15).
/// For @integration tests, this in-memory implementation is sufficient to
/// validate topology logic.
pub struct NamespaceShardMapStore {
    maps: RwLock<HashMap<String, NamespaceShardMap>>,
    /// Per-tenant shard bounds (cluster admin configuration).
    tenant_bounds: RwLock<HashMap<String, TenantShardBounds>>,
    /// Failure injection: if set, the N-th shard creation fails (ADV-033-1 testing).
    fail_at_shard: RwLock<Option<u32>>,
}

impl NamespaceShardMapStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            maps: RwLock::new(HashMap::new()),
            tenant_bounds: RwLock::new(HashMap::new()),
            fail_at_shard: RwLock::new(None),
        }
    }

    /// Inject a failure at the N-th shard during creation (1-indexed).
    /// Used for testing atomic rollback (ADV-033-1).
    pub fn inject_failure_at_shard(&self, shard_number: u32) {
        *self.fail_at_shard.write().unwrap() = Some(shard_number);
    }

    /// Clear failure injection.
    pub fn clear_failure_injection(&self) {
        *self.fail_at_shard.write().unwrap() = None;
    }

    /// Create a namespace with the computed shard topology.
    ///
    /// Returns the created `NamespaceShardMap` or an error if the namespace
    /// already exists or is currently being created.
    pub fn create_namespace(
        &self,
        namespace_id: &str,
        tenant_id: OrgId,
        config: &ShardTopologyConfig,
        active_nodes: &[NodeId],
        requested_shards: Option<u32>,
    ) -> Result<NamespaceShardMap, ControlError> {
        let mut maps = self.maps.write().unwrap();

        // ADV-033-1: reject concurrent creation.
        if let Some(existing) = maps.get(namespace_id) {
            if existing.state == NamespaceCreationState::Creating {
                return Err(ControlError::Rejected(
                    "namespace creation in progress".into(),
                ));
            }
            return Err(ControlError::AlreadyExists(format!(
                "namespace {namespace_id}"
            )));
        }

        // Determine shard count.
        let shard_count = if let Some(requested) = requested_shards {
            // Validate against tenant bounds if set.
            let bounds = self.tenant_bounds.read().unwrap();
            if let Some(b) = bounds.get(&tenant_id.0.to_string()) {
                if requested > b.max_shards {
                    return Err(ControlError::Rejected(format!(
                        "initial_shards exceeds tenant ceiling ({})",
                        b.max_shards
                    )));
                }
                requested.max(b.min_shards)
            } else {
                requested
            }
        } else {
            compute_initial_shards(config, active_nodes.len() as u32)
        };

        // ADV-033-1: failure injection — simulate partial Raft group failure.
        let fail_at = *self.fail_at_shard.read().unwrap();
        if let Some(fail_shard) = fail_at {
            if fail_shard <= shard_count {
                return Err(ControlError::Rejected(format!(
                    "namespace creation failed: shard {} did not reach quorum",
                    fail_shard
                )));
            }
        }

        let shards = compute_shard_ranges(shard_count, active_nodes);

        let map = NamespaceShardMap {
            namespace_id: namespace_id.to_owned(),
            tenant_id,
            version: 1,
            shards,
            state: NamespaceCreationState::Active,
        };

        maps.insert(namespace_id.to_owned(), map.clone());
        Ok(map)
    }

    /// Get the shard map for a namespace, validating tenant authorization (ADV-033-9).
    pub fn get(
        &self,
        namespace_id: &str,
        caller_tenant: OrgId,
    ) -> Result<NamespaceShardMap, ControlError> {
        let maps = self.maps.read().unwrap();
        let map = maps
            .get(namespace_id)
            .ok_or_else(|| ControlError::NotFound(format!("namespace {namespace_id}")))?;

        if map.tenant_id != caller_tenant {
            return Err(ControlError::NotPermitted(
                "PermissionDenied".into(),
            ));
        }

        Ok(map.clone())
    }

    /// Set per-tenant shard bounds (cluster admin operation).
    pub fn set_tenant_bounds(&self, tenant_id: &str, bounds: TenantShardBounds) {
        self.tenant_bounds
            .write()
            .unwrap()
            .insert(tenant_id.to_owned(), bounds);
    }

    /// Evaluate and execute ratio-floor splits for a namespace (ADR-033 §3).
    ///
    /// If the shards-per-node ratio is below `ratio_floor`, splits the
    /// largest shard(s) until the target count is reached (capped by `shard_cap`).
    /// Returns the new shard count, or `None` if no splits were needed.
    pub fn evaluate_ratio_floor(
        &self,
        namespace_id: &str,
        config: &ShardTopologyConfig,
        active_nodes: &[NodeId],
    ) -> Option<u32> {
        let mut maps = self.maps.write().unwrap();
        let map = maps.get_mut(namespace_id)?;

        let current = map.shards.len() as u32;
        let target = check_ratio_floor(config, current, active_nodes.len() as u32)?;

        // Split shards until we reach the target.
        // Each split divides the largest shard's key range at its midpoint.
        while (map.shards.len() as u32) < target {
            // Find the shard with the widest range (simplification: pick the first
            // shard that hasn't been split yet in this cycle, or the one with the
            // widest range).
            let split_idx = find_widest_shard(&map.shards);

            let old = map.shards.remove(split_idx);
            let midpoint = midpoint_256(&old.range_start, &old.range_end);

            // Leader placement: round-robin across active nodes.
            let new_leader_idx = map.shards.len() % active_nodes.len();
            let new_leader = active_nodes.get(new_leader_idx).copied().unwrap_or(NodeId(1));

            let left = ShardRange {
                shard_id: old.shard_id,
                range_start: old.range_start,
                range_end: midpoint,
                leader_node: old.leader_node,
            };
            let right = ShardRange {
                shard_id: ShardId(uuid::Uuid::new_v4()),
                range_start: midpoint,
                range_end: old.range_end,
                leader_node: new_leader,
            };

            map.shards.push(left);
            map.shards.push(right);

            // Keep sorted by range_start.
            map.shards.sort_by(|a, b| a.range_start.cmp(&b.range_start));
        }

        map.version += 1;
        Some(map.shards.len() as u32)
    }

    /// Get the current shard count for a namespace (unauthenticated, internal use).
    pub fn shard_count(&self, namespace_id: &str) -> Option<u32> {
        let maps = self.maps.read().unwrap();
        maps.get(namespace_id).map(|m| m.shards.len() as u32)
    }

    /// Check whether a namespace exists in the store (any state).
    pub fn namespace_exists(&self, namespace_id: &str) -> bool {
        self.maps.read().unwrap().contains_key(namespace_id)
    }

    /// Create an alias: lookups for `alias` resolve to `target`.
    pub fn alias(&self, alias: &str, target: &str) {
        let maps = self.maps.read().unwrap();
        if let Some(map) = maps.get(target) {
            let aliased = map.clone();
            drop(maps);
            self.maps.write().unwrap().insert(alias.to_owned(), aliased);
        }
    }

    /// Insert a namespace in Creating state (for testing ADV-033-1).
    pub fn insert_creating(&self, namespace_id: &str, tenant_id: OrgId) {
        let map = NamespaceShardMap {
            namespace_id: namespace_id.to_owned(),
            tenant_id,
            version: 0,
            shards: Vec::new(),
            state: NamespaceCreationState::Creating,
        };
        self.maps
            .write()
            .unwrap()
            .insert(namespace_id.to_owned(), map);
    }
}

/// Find the shard with the widest key range.
fn find_widest_shard(shards: &[ShardRange]) -> usize {
    shards
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            range_width(&a.range_start, &a.range_end)
                .cmp(&range_width(&b.range_start, &b.range_end))
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Approximate range width for comparison (first 8 bytes as u64).
fn range_width(start: &[u8; 32], end: &[u8; 32]) -> u64 {
    let s = u64::from_be_bytes(start[..8].try_into().unwrap());
    let e = u64::from_be_bytes(end[..8].try_into().unwrap());
    e.saturating_sub(s)
}

/// Compute midpoint of two 256-bit values (big-endian).
fn midpoint_256(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut result = [0u8; 32];
    let mut carry: u16 = 0;

    // Add a + b byte-by-byte from LSB to MSB.
    let mut sum = [0u16; 32];
    for i in (0..32).rev() {
        let s = a[i] as u16 + b[i] as u16 + carry;
        sum[i] = s & 0xFF;
        carry = s >> 8;
    }

    // Divide by 2 (shift right by 1).
    let mut borrow: u16 = carry; // carry from addition becomes MSB
    for i in 0..32 {
        let val = (borrow << 8) | sum[i];
        result[i] = (val >> 1) as u8;
        borrow = val & 1;
    }

    result
}

impl Default for NamespaceShardMapStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_initial_shards_default_3_nodes() {
        let config = ShardTopologyConfig::default();
        assert_eq!(compute_initial_shards(&config, 3), 9);
    }

    #[test]
    fn compute_initial_shards_floor() {
        let config = ShardTopologyConfig::default();
        // 1 node: 3 * 1 = 3, max(min(3, 64), 3) = 3
        assert_eq!(compute_initial_shards(&config, 1), 3);
    }

    #[test]
    fn compute_initial_shards_cap() {
        let config = ShardTopologyConfig::default();
        // 100 nodes: 3 * 100 = 300, max(min(300, 64), 3) = 64
        assert_eq!(compute_initial_shards(&config, 100), 64);
    }

    #[test]
    fn compute_initial_shards_custom_multiplier() {
        let config = ShardTopologyConfig {
            multiplier: 2,
            ..Default::default()
        };
        // 5 nodes: 2 * 5 = 10, max(min(10, 64), 3) = 10
        assert_eq!(compute_initial_shards(&config, 5), 10);
    }

    #[test]
    fn shard_ranges_cover_full_keyspace() {
        let nodes = vec![NodeId(1), NodeId(2), NodeId(3)];
        let ranges = compute_shard_ranges(9, &nodes);
        assert_eq!(ranges.len(), 9);

        // First range starts at 0x00..00.
        assert_eq!(ranges[0].range_start, [0u8; 32]);
        // Last range ends at 0xFF..FF.
        assert_eq!(ranges[8].range_end, [0xFF; 32]);

        // Ranges are contiguous: each range_end == next range_start.
        for i in 0..8 {
            assert_eq!(
                ranges[i].range_end, ranges[i + 1].range_start,
                "gap between range {i} and {}",
                i + 1
            );
        }
    }

    #[test]
    fn route_to_shard_finds_correct_shard() {
        let nodes = vec![NodeId(1), NodeId(2), NodeId(3)];
        let shards = compute_shard_ranges(3, &nodes);
        let map = NamespaceShardMap {
            namespace_id: "test".into(),
            tenant_id: OrgId(uuid::Uuid::from_u128(1)),
            version: 1,
            shards,
            state: NamespaceCreationState::Active,
        };

        // Key 0x00..00 should be in shard 0.
        let key_low = [0u8; 32];
        assert_eq!(route_to_shard(&map, &key_low), Some(map.shards[0].shard_id));

        // Key 0xFF..FF should be in the last shard.
        let key_high = [0xFF; 32];
        assert_eq!(route_to_shard(&map, &key_high), Some(map.shards[2].shard_id));
    }

    #[test]
    fn ratio_floor_violated() {
        let config = ShardTopologyConfig::default();
        // 9 shards, 7 nodes: ratio = 1.29, floor = 1.5 → violated
        let result = check_ratio_floor(&config, 9, 7);
        assert_eq!(result, Some(11)); // ceil(1.5 * 7) = 11
    }

    #[test]
    fn ratio_floor_satisfied() {
        let config = ShardTopologyConfig::default();
        // 9 shards, 4 nodes: ratio = 2.25, floor = 1.5 → satisfied
        assert_eq!(check_ratio_floor(&config, 9, 4), None);
    }

    #[test]
    fn ratio_floor_respects_cap() {
        let config = ShardTopologyConfig::default();
        // 9 shards, 50 nodes: ratio = 0.18, target = ceil(1.5*50) = 75, cap = 64
        let result = check_ratio_floor(&config, 9, 50);
        assert_eq!(result, Some(64));
    }

    #[test]
    fn max_concurrent_splits_formula() {
        assert_eq!(max_concurrent_splits(3), 1);    // max(1, 3/5) = 1
        assert_eq!(max_concurrent_splits(10), 2);   // max(1, 10/5) = 2
        assert_eq!(max_concurrent_splits(50), 10);  // max(1, 50/5) = 10
    }
}
