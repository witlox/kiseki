//! Affinity pool management (I-C3, I-C4).
//!
//! 2026-05-31 amendment (ADR-024 §"three-tier durability by size band"):
//! pools now carry a [`PoolRole`] tag (`Chunk` / `Metadata` / `Inline`)
//! and per-pool size-band thresholds. The gateway uses
//! [`select_pool_for_write`] to route a PUT by encrypted-payload size:
//!
//! - `size ≤ inline_threshold_bytes` → an `Inline` pool (no chunk fan,
//!   bytes ride in the Raft delta per ADR-030).
//! - `inline_threshold_bytes < size ≤ replication_ceiling_bytes` →
//!   a `Chunk` pool with [`DurabilityStrategy::Replication`].
//! - `size > replication_ceiling_bytes` → a `Chunk` pool with
//!   [`DurabilityStrategy::ErasureCoding`].

/// Durability strategy per pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DurabilityStrategy {
    /// Erasure coding (default for the cold / large-tier band).
    ErasureCoding {
        /// Number of data shards.
        data_shards: u8,
        /// Number of parity shards.
        parity_shards: u8,
    },
    /// N-copy replication (medium-tier band; also the hot tier for the
    /// ADR-048 slab-EC compactor when configured).
    Replication {
        /// Number of copies.
        copies: u8,
    },
    /// Inline payload in Raft delta (ADR-030, ADR-024 amendment).
    /// Durability is via the per-shard Raft group (`R-3` against the
    /// shard's voters); no chunk-fabric fan-out, no separate copy
    /// count. The variant carries no fields because the durability
    /// factor is inherited from the shard's Raft topology.
    Inline,
}

impl Default for DurabilityStrategy {
    fn default() -> Self {
        Self::ErasureCoding {
            data_shards: 4,
            parity_shards: 2,
        }
    }
}

impl DurabilityStrategy {
    /// Per-byte multiplier this strategy imposes on the cluster's
    /// physical storage budget (ADR-045 §D6 quota accounting).
    ///
    /// - `ErasureCoding { data, parity }` → `(data + parity) / data`
    /// - `Replication { copies }` → `copies as f64`
    /// - `Inline` → `1.0` for cold-tier accounting (the per-shard
    ///   Raft replication is accounted under the metadata pool's
    ///   own quota, not against the pool that issued the write).
    #[must_use]
    pub fn storage_multiplier(self) -> f64 {
        match self {
            Self::ErasureCoding {
                data_shards,
                parity_shards,
            } => {
                f64::from(u32::from(data_shards) + u32::from(parity_shards))
                    / f64::from(data_shards.max(1))
            }
            Self::Replication { copies } => f64::from(copies),
            Self::Inline => 1.0,
        }
    }
}

/// What kind of writes a pool is intended to receive
/// (ADR-024 2026-05-31 amendment).
///
/// `Chunk` pools hold encrypted chunk fragments — the "data tier".
/// `Metadata` pools hold the per-shard fjall + `small/objects.redb`
/// — the structural metadata + inline content (ADR-030).
/// `Inline` is the role used by the runtime to identify the *target*
/// pool name for an inline write, even though the actual storage lives
/// on the metadata tier; it's a write-routing tag, not a device tag.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum PoolRole {
    /// Holds chunk fragments (the data tier; default for back-compat
    /// with pools created before the role tag existed).
    #[default]
    Chunk,
    /// Holds fjall + small/objects.redb (the metadata + inline tier).
    Metadata,
    /// Write-routing tag for the inline band — chunks logically destined
    /// for the inline pool route into the metadata pool's `small_store`.
    /// This role exists so a namespace's tier-policy can declare an
    /// "inline pool" without conflating it with the physical metadata
    /// pool that actually holds the bytes.
    Inline,
}

/// Device class for pool-level placement decisions.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum DeviceClass {
    /// `NVMe` SSD — lowest latency.
    NvmeSsd,
    /// SATA/SAS SSD.
    Ssd,
    /// Rotational hard drive — bulk capacity.
    Hdd,
    /// Mixed or unspecified device types (also the `Default`).
    #[default]
    Mixed,
}

/// Hard upper bound on inline payload size (ADR-030 `INLINE_CEILING`).
/// The runtime clamps any per-pool `inline_threshold_bytes` to this value
/// regardless of operator config: payloads above 64 KiB break the
/// per-shard Raft throughput guard (ADR-030 §3 SF-ADV-1) and grow Raft
/// snapshots unboundedly.
pub const INLINE_CEILING_BYTES: u64 = 64 * 1024;

/// Hard lower bound on inline payload size (ADR-030 `INLINE_FLOOR`).
/// Metadata-like payloads (empty files, symlinks, xattrs) always
/// inline at or above this size.
pub const INLINE_FLOOR_BYTES: u64 = 128;

/// Default cluster-wide inline threshold (ADR-030 amendment).
/// 16 KiB covers the typical "small" workload (xattrs, symlinks, small
/// JSON, FS metadata) without exceeding the per-shard Raft throughput
/// budget for medium files. Per-pool override via [`AffinityPool::inline_threshold_bytes`].
pub const DEFAULT_INLINE_THRESHOLD_BYTES: u64 = 16 * 1024;

/// Default cluster-wide replication ceiling (ADR-024 amendment).
/// 4 MiB matches the workload-shape break-even between R-3 fan-out
/// cost and EC storage saving. Per-pool override via [`AffinityPool::replication_ceiling_bytes`].
pub const DEFAULT_REPLICATION_CEILING_BYTES: u64 = 4 * 1024 * 1024;

/// An affinity pool — group of storage devices sharing a device class.
///
/// 2026-05-31 amendment: pools now carry [`PoolRole`] +
/// `inline_threshold_bytes` + `replication_ceiling_bytes` so the
/// gateway's [`select_pool_for_write`] can route a PUT to the right
/// pool by size band (ADR-024 §"three-tier durability").
///
/// `Default` exists so external struct-literal call sites can use
/// `..Default::default()` for the amendment's new fields without
/// touching every line — `name` defaults to empty and MUST be
/// overridden by the caller.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AffinityPool {
    /// Pool name (e.g., `"fast-nvme"`, `"bulk-nvme"`).
    pub name: String,
    /// Durability strategy for this pool.
    pub durability: DurabilityStrategy,
    /// Maximum capacity in bytes.
    pub capacity_bytes: u64,
    /// Current used bytes.
    pub used_bytes: u64,
    /// Device class for this pool.
    pub device_class: DeviceClass,
    /// Devices in this pool.
    pub devices: Vec<PoolDevice>,
    /// What kind of writes this pool is intended to receive
    /// (ADR-024 amendment). Defaults to `Chunk` for back-compat with
    /// pools created before the role tag existed.
    #[serde(default)]
    pub role: PoolRole,
    /// Encrypted-payload size at or below which a write to this pool
    /// goes inline (ADR-030). Only meaningful when [`Self::durability`]
    /// is [`DurabilityStrategy::Inline`] OR when the pool is the
    /// inline-tier target in a namespace's tier-policy chain.
    /// Clamped to `[INLINE_FLOOR_BYTES, INLINE_CEILING_BYTES]` at read time
    /// via [`Self::effective_inline_threshold`].
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold_bytes: u64,
    /// Encrypted-payload size at or below which a write routes to a
    /// replicated pool (medium tier). Above this, the write routes to an
    /// EC pool (cold/large tier). Per-pool override of the cluster
    /// default. Set on a `Chunk` pool to be its own write-band ceiling.
    #[serde(default = "default_replication_ceiling")]
    pub replication_ceiling_bytes: u64,
    /// ADR-048 §"Decision" — when `true`, the slab-EC compactor
    /// picks up chunks landed in this pool and migrates them into
    /// cold-tier slabs. Only meaningful for `Replication` pools
    /// (replicating then migrating to EC is the win); `false` for
    /// EC pools (already EC, nothing to migrate) and `Inline` pools
    /// (no chunk fabric copy to begin with). Default `false` for
    /// back-compat with pre-amendment records.
    #[serde(default)]
    pub requires_migration: bool,
}

const fn default_inline_threshold() -> u64 {
    DEFAULT_INLINE_THRESHOLD_BYTES
}

const fn default_replication_ceiling() -> u64 {
    DEFAULT_REPLICATION_CEILING_BYTES
}

/// A device within a pool.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PoolDevice {
    /// Device identifier (e.g., `"d1"`).
    pub id: String,
    /// Whether the device is online.
    pub online: bool,
}

impl AffinityPool {
    /// Create a new pool.
    #[must_use]
    /// Create a new pool with no devices.
    pub fn new(name: &str, durability: DurabilityStrategy, capacity_bytes: u64) -> Self {
        Self {
            name: name.to_owned(),
            durability,
            capacity_bytes,
            used_bytes: 0,
            device_class: DeviceClass::Mixed,
            devices: Vec::new(),
            role: PoolRole::default(),
            inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
            replication_ceiling_bytes: DEFAULT_REPLICATION_CEILING_BYTES,
            requires_migration: false,
        }
    }

    /// Set the pool's [`PoolRole`] (builder pattern).
    #[must_use]
    pub const fn with_role(mut self, role: PoolRole) -> Self {
        self.role = role;
        self
    }

    /// Set the pool's `inline_threshold_bytes` (builder pattern).
    /// Clamped at read time via [`Self::effective_inline_threshold`].
    #[must_use]
    pub const fn with_inline_threshold(mut self, bytes: u64) -> Self {
        self.inline_threshold_bytes = bytes;
        self
    }

    /// Set the pool's `replication_ceiling_bytes` (builder pattern).
    #[must_use]
    pub const fn with_replication_ceiling(mut self, bytes: u64) -> Self {
        self.replication_ceiling_bytes = bytes;
        self
    }

    /// Effective inline threshold — `inline_threshold_bytes` clamped to
    /// `[INLINE_FLOOR_BYTES, INLINE_CEILING_BYTES]`.
    #[must_use]
    pub fn effective_inline_threshold(&self) -> u64 {
        self.inline_threshold_bytes
            .clamp(INLINE_FLOOR_BYTES, INLINE_CEILING_BYTES)
    }

    /// Create a pool with `n` auto-named online devices.
    #[must_use]
    pub fn with_devices(mut self, n: usize) -> Self {
        self.devices = (1..=n)
            .map(|i| PoolDevice {
                id: format!("d{i}"),
                online: true,
            })
            .collect();
        self
    }

    /// Set a device online/offline by ID.
    pub fn set_device_online(&mut self, device_id: &str, online: bool) {
        if let Some(d) = self.devices.iter_mut().find(|d| d.id == device_id) {
            d.online = online;
        }
    }

    /// Available space in the pool.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.capacity_bytes.saturating_sub(self.used_bytes)
    }

    /// Whether the pool has room for `size` bytes.
    #[must_use]
    pub fn has_capacity(&self, size: u64) -> bool {
        self.available_bytes() >= size
    }

    /// Set the device class for this pool (builder pattern).
    #[must_use]
    pub fn with_device_class(mut self, class: DeviceClass) -> Self {
        self.device_class = class;
        self
    }
}

/// Which size band a write of `data_size` falls into (ADR-024 amendment).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteSizeBand {
    /// `data_size ≤ inline_threshold` — bytes ride in the Raft delta
    /// (ADR-030 §4 inline path).
    Inline,
    /// `inline_threshold < data_size ≤ replication_ceiling` — replication
    /// (R-3 default) on the chunk fabric.
    Replicated,
    /// `data_size > replication_ceiling` — EC on the chunk fabric.
    Erasure,
}

/// Determine which size band a write falls into, given the inline
/// threshold and replication ceiling that govern this routing decision.
/// Used by [`select_pool_for_write`] and by the gateway when emitting
/// the pool-selection telemetry breadcrumb.
#[must_use]
pub fn classify_write_size(
    data_size: u64,
    inline_threshold: u64,
    replication_ceiling: u64,
) -> WriteSizeBand {
    if data_size <= inline_threshold {
        WriteSizeBand::Inline
    } else if data_size <= replication_ceiling {
        WriteSizeBand::Replicated
    } else {
        WriteSizeBand::Erasure
    }
}

/// Per-namespace policy declaring which pools serve each size band
/// (ADR-045 amendment cross-ref to ADR-024). A namespace without a
/// policy falls back to the cluster default chain
/// `[inline, replicated, ec]` resolved against the highest-class
/// pools available.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NamespaceTierPolicy {
    /// Pool name for the inline band (`Inline` role). `None` →
    /// cluster default.
    pub inline: Option<String>,
    /// Pool name for the replicated band (`Chunk` role with
    /// [`DurabilityStrategy::Replication`]). `None` → cluster default.
    pub replicated: Option<String>,
    /// Pool name for the EC band (`Chunk` role with
    /// [`DurabilityStrategy::ErasureCoding`]). `None` → cluster default.
    pub ec: Option<String>,
}

/// Select the pool for a write, **routed by size band** per the
/// 2026-05-31 ADR-024 amendment.
///
/// Resolution order:
///
/// 1. Classify the write into a [`WriteSizeBand`] using the band-leader
///    pool's `inline_threshold_bytes` + `replication_ceiling_bytes`. The
///    band-leader is the inline pool if one exists; otherwise the
///    replicated pool; otherwise cluster defaults
///    ([`DEFAULT_INLINE_THRESHOLD_BYTES`], [`DEFAULT_REPLICATION_CEILING_BYTES`]).
/// 2. If the namespace's `tier_policy` names a pool for that band,
///    return it (when it exists in `pools` and has capacity for `data_size`).
/// 3. Otherwise, pick the highest-class pool of the matching role +
///    durability shape for the band.
///
/// `preferred_class` is honoured if present and a pool of that class
/// exists for the chosen band — otherwise ignored.
///
/// Returns `None` only when `pools` is empty.
#[must_use]
pub fn select_pool_for_write<'a>(
    pools: &'a [AffinityPool],
    data_size: u64,
    tier_policy: Option<&NamespaceTierPolicy>,
    preferred_class: Option<DeviceClass>,
) -> Option<&'a AffinityPool> {
    if pools.is_empty() {
        return None;
    }

    // (1) Classify by size.
    let (inline_threshold, replication_ceiling) = band_thresholds(pools, tier_policy);
    let band = classify_write_size(data_size, inline_threshold, replication_ceiling);

    // (2) Honour explicit tier-policy for the band.
    if let Some(policy) = tier_policy {
        let policy_pool_name = match band {
            WriteSizeBand::Inline => policy.inline.as_deref(),
            WriteSizeBand::Replicated => policy.replicated.as_deref(),
            WriteSizeBand::Erasure => policy.ec.as_deref(),
        };
        if let Some(name) = policy_pool_name {
            if let Some(pool) = pools.iter().find(|p| p.name == name) {
                if pool.has_capacity(data_size) {
                    return Some(pool);
                }
            }
        }
    }

    // (3) Auto-route by band → role + durability shape.
    let match_band = |p: &&AffinityPool| -> bool {
        match band {
            WriteSizeBand::Inline => {
                p.role == PoolRole::Inline || matches!(p.durability, DurabilityStrategy::Inline)
            }
            WriteSizeBand::Replicated => {
                p.role == PoolRole::Chunk
                    && matches!(p.durability, DurabilityStrategy::Replication { .. })
            }
            WriteSizeBand::Erasure => {
                p.role == PoolRole::Chunk
                    && matches!(p.durability, DurabilityStrategy::ErasureCoding { .. })
            }
        }
    };

    // Prefer the requested class if a matching-band pool of that class
    // exists; else any matching-band pool; else fall back to any pool
    // with capacity (preserves the pre-amendment semantics for
    // single-pool clusters that haven't been migrated yet).
    if let Some(class) = preferred_class {
        if let Some(pool) = pools
            .iter()
            .find(|p| match_band(p) && p.device_class == class && p.has_capacity(data_size))
        {
            return Some(pool);
        }
    }
    if let Some(pool) = pools
        .iter()
        .find(|p| match_band(p) && p.has_capacity(data_size))
    {
        return Some(pool);
    }

    // Back-compat: single-pool clusters or unmigrated configs fall back
    // to first pool with capacity, then to first pool.
    pools
        .iter()
        .find(|p| p.has_capacity(data_size))
        .or_else(|| pools.first())
}

fn band_thresholds(
    pools: &[AffinityPool],
    tier_policy: Option<&NamespaceTierPolicy>,
) -> (u64, u64) {
    // Use the inline pool's thresholds when one is named (or detected by
    // role) so per-namespace overrides take effect. Falls through to
    // cluster defaults otherwise.
    let inline_pool = tier_policy
        .and_then(|p| p.inline.as_deref())
        .and_then(|n| pools.iter().find(|p| p.name == n))
        .or_else(|| {
            pools.iter().find(|p| {
                p.role == PoolRole::Inline || matches!(p.durability, DurabilityStrategy::Inline)
            })
        });
    let inline_threshold = inline_pool.map_or(
        DEFAULT_INLINE_THRESHOLD_BYTES,
        AffinityPool::effective_inline_threshold,
    );

    let replicated_pool = tier_policy
        .and_then(|p| p.replicated.as_deref())
        .and_then(|n| pools.iter().find(|p| p.name == n))
        .or_else(|| {
            pools.iter().find(|p| {
                p.role == PoolRole::Chunk
                    && matches!(p.durability, DurabilityStrategy::Replication { .. })
            })
        });
    let replication_ceiling = replicated_pool.map_or(DEFAULT_REPLICATION_CEILING_BYTES, |p| {
        p.replication_ceiling_bytes
    });

    (inline_threshold, replication_ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pools() -> Vec<AffinityPool> {
        vec![
            AffinityPool::new("nvme-fast", DurabilityStrategy::default(), 1_000_000)
                .with_device_class(DeviceClass::NvmeSsd),
            AffinityPool::new("ssd-tier", DurabilityStrategy::default(), 10_000_000)
                .with_device_class(DeviceClass::Ssd),
            AffinityPool::new("hdd-bulk", DurabilityStrategy::default(), 100_000_000)
                .with_device_class(DeviceClass::Hdd),
        ]
    }

    /// Three-tier setup mirroring the ADR-024 amendment's recommended
    /// default chain: an inline pool, a replicated pool (medium tier),
    /// and an EC pool (cold tier). Used by the band-routing tests
    /// below.
    fn make_tiered_pools() -> Vec<AffinityPool> {
        vec![
            AffinityPool::new("inline-meta", DurabilityStrategy::Inline, 1_000_000_000)
                .with_role(PoolRole::Inline)
                .with_device_class(DeviceClass::NvmeSsd)
                .with_inline_threshold(16 * 1024),
            AffinityPool::new(
                "fast-r3",
                DurabilityStrategy::Replication { copies: 3 },
                10_000_000_000,
            )
            .with_role(PoolRole::Chunk)
            .with_device_class(DeviceClass::NvmeSsd)
            .with_replication_ceiling(4 * 1024 * 1024),
            AffinityPool::new(
                "cold-ec",
                DurabilityStrategy::ErasureCoding {
                    data_shards: 4,
                    parity_shards: 2,
                },
                100_000_000_000,
            )
            .with_role(PoolRole::Chunk)
            .with_device_class(DeviceClass::Hdd),
        ]
    }

    #[test]
    fn small_write_prefers_nvme() {
        let pools = make_pools();
        // Untiered pools default to Chunk role + EC durability, so a
        // 4 KiB write falls into the EC band and routes to the
        // first matching-band pool (or, since none match perfectly,
        // first pool with capacity). Pre-amendment behaviour
        // (nvme-fast wins) is preserved by the back-compat fallback.
        let selected = select_pool_for_write(&pools, 4096, None, None).unwrap();
        assert_eq!(selected.device_class, DeviceClass::NvmeSsd);
    }

    #[test]
    fn large_write_routes_to_ec_chunk_pool() {
        let pools = make_pools();
        let selected = select_pool_for_write(&pools, 10 * 1024 * 1024, None, None).unwrap();
        // EC-band, all pools default Chunk-role-EC; first matching pool
        // is the NVMe one (no HDD-class preference without `preferred_class`).
        assert!(matches!(
            selected.durability,
            DurabilityStrategy::ErasureCoding { .. }
        ));
    }

    #[test]
    fn preferred_class_steers_within_band() {
        let pools = make_pools();
        let selected =
            select_pool_for_write(&pools, 10 * 1024 * 1024, None, Some(DeviceClass::Hdd)).unwrap();
        assert_eq!(selected.device_class, DeviceClass::Hdd);
    }

    #[test]
    fn fallback_to_first_when_no_match() {
        let pools = vec![AffinityPool::new(
            "only-mixed",
            DurabilityStrategy::default(),
            1_000_000,
        )];
        // Small write with no NVMe/SSD pool — should fall back to first.
        let selected = select_pool_for_write(&pools, 1024, None, None).unwrap();
        assert_eq!(selected.name, "only-mixed");
    }

    // ---------------------------------------------------------------
    // ADR-024 amendment §"three-tier durability by size band"
    // ---------------------------------------------------------------

    #[test]
    fn inline_band_routes_to_inline_pool_by_role() {
        let pools = make_tiered_pools();
        // 8 KiB ≤ 16 KiB inline threshold → Inline band.
        let selected = select_pool_for_write(&pools, 8 * 1024, None, None).unwrap();
        assert_eq!(selected.name, "inline-meta");
        assert_eq!(selected.role, PoolRole::Inline);
        assert!(matches!(selected.durability, DurabilityStrategy::Inline));
    }

    #[test]
    fn replicated_band_routes_to_r3_pool() {
        let pools = make_tiered_pools();
        // 100 KiB > 16 KiB but ≤ 4 MiB → Replicated band.
        let selected = select_pool_for_write(&pools, 100 * 1024, None, None).unwrap();
        assert_eq!(selected.name, "fast-r3");
        assert_eq!(selected.role, PoolRole::Chunk);
        assert!(matches!(
            selected.durability,
            DurabilityStrategy::Replication { copies: 3 }
        ));
    }

    #[test]
    fn ec_band_routes_to_ec_pool() {
        let pools = make_tiered_pools();
        // 10 MiB > 4 MiB → Erasure band.
        let selected = select_pool_for_write(&pools, 10 * 1024 * 1024, None, None).unwrap();
        assert_eq!(selected.name, "cold-ec");
        assert!(matches!(
            selected.durability,
            DurabilityStrategy::ErasureCoding { .. }
        ));
    }

    #[test]
    fn classify_write_size_boundaries() {
        // Exactly at threshold = inline.
        assert_eq!(
            classify_write_size(16 * 1024, 16 * 1024, 4 * 1024 * 1024),
            WriteSizeBand::Inline
        );
        // One byte over inline threshold = replicated.
        assert_eq!(
            classify_write_size(16 * 1024 + 1, 16 * 1024, 4 * 1024 * 1024),
            WriteSizeBand::Replicated
        );
        // Exactly at replication ceiling = replicated.
        assert_eq!(
            classify_write_size(4 * 1024 * 1024, 16 * 1024, 4 * 1024 * 1024),
            WriteSizeBand::Replicated
        );
        // One byte over replication ceiling = EC.
        assert_eq!(
            classify_write_size(4 * 1024 * 1024 + 1, 16 * 1024, 4 * 1024 * 1024),
            WriteSizeBand::Erasure
        );
    }

    #[test]
    fn namespace_tier_policy_overrides_default() {
        let pools = make_tiered_pools();
        // Force replicated-band write to ec pool via tier policy.
        let policy = NamespaceTierPolicy {
            inline: None,
            replicated: Some("cold-ec".into()),
            ec: None,
        };
        let selected = select_pool_for_write(&pools, 100 * 1024, Some(&policy), None).unwrap();
        assert_eq!(selected.name, "cold-ec");
    }

    #[test]
    fn inline_pool_threshold_drives_band_classification() {
        let mut pools = make_tiered_pools();
        // Bump the inline pool's threshold to 64 KiB.
        pools[0].inline_threshold_bytes = 64 * 1024;
        // 32 KiB write — was Replicated under 16 KiB threshold; now Inline.
        let selected = select_pool_for_write(&pools, 32 * 1024, None, None).unwrap();
        assert_eq!(selected.name, "inline-meta");
    }

    #[test]
    fn effective_inline_threshold_clamps_to_ceiling() {
        let p = AffinityPool::new("over", DurabilityStrategy::Inline, 1)
            .with_inline_threshold(1024 * 1024);
        assert_eq!(p.effective_inline_threshold(), INLINE_CEILING_BYTES);
    }

    #[test]
    fn effective_inline_threshold_clamps_to_floor() {
        let p = AffinityPool::new("under", DurabilityStrategy::Inline, 1).with_inline_threshold(1);
        assert_eq!(p.effective_inline_threshold(), INLINE_FLOOR_BYTES);
    }

    #[test]
    fn storage_multiplier_replication() {
        assert!(
            (DurabilityStrategy::Replication { copies: 3 }.storage_multiplier() - 3.0).abs() < 1e-9
        );
    }

    #[test]
    fn storage_multiplier_ec() {
        let mult = DurabilityStrategy::ErasureCoding {
            data_shards: 4,
            parity_shards: 2,
        }
        .storage_multiplier();
        assert!((mult - 1.5).abs() < 1e-9);
    }

    #[test]
    fn storage_multiplier_inline_is_one() {
        assert!((DurabilityStrategy::Inline.storage_multiplier() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn empty_pools_returns_none() {
        assert!(select_pool_for_write(&[], 1024, None, None).is_none());
    }

    // ---------------------------------------------------------------
    // Scenario: Pool redirection stays within same device class
    // When primary NVMe pool is Critical, redirect to a healthy
    // NVMe sibling — never to a HDD pool.
    // ---------------------------------------------------------------
    #[test]
    fn pool_redirection_stays_within_device_class() {
        use crate::device::{CapacityThresholds, PoolHealth};

        let pools = [
            AffinityPool::new("fast-nvme-a", DurabilityStrategy::default(), 1_000_000)
                .with_device_class(DeviceClass::NvmeSsd),
            AffinityPool::new("fast-nvme-b", DurabilityStrategy::default(), 1_000_000)
                .with_device_class(DeviceClass::NvmeSsd),
            AffinityPool::new("bulk-hdd", DurabilityStrategy::default(), 10_000_000)
                .with_device_class(DeviceClass::Hdd),
        ];

        // Pool A is Critical (86% usage for NVMe thresholds).
        let thresholds = CapacityThresholds::nvme();
        assert_eq!(thresholds.health(86), PoolHealth::Critical);

        // When the primary pool is Critical, select a healthy same-class sibling.
        let healthy_same_class: Vec<&AffinityPool> = pools
            .iter()
            .filter(|p| p.device_class == DeviceClass::NvmeSsd && p.name != "fast-nvme-a")
            .collect();

        assert!(!healthy_same_class.is_empty());
        let redirected = healthy_same_class[0];
        assert_eq!(redirected.device_class, DeviceClass::NvmeSsd);
        assert_eq!(redirected.name, "fast-nvme-b");
        // Verify we never redirect to HDD.
        assert_ne!(redirected.device_class, DeviceClass::Hdd);
    }

    // ---------------------------------------------------------------
    // Scenario: No sibling pool available — ENOSPC
    // Only NVMe pool is Critical, no same-class sibling exists.
    // ---------------------------------------------------------------
    #[test]
    fn no_sibling_pool_returns_enospc() {
        use crate::device::{CapacityThresholds, PoolHealth};
        use crate::error::ChunkError;

        let pools = [
            AffinityPool::new("fast-nvme", DurabilityStrategy::default(), 1_000_000)
                .with_device_class(DeviceClass::NvmeSsd),
        ];

        // The only NVMe pool is Critical.
        let thresholds = CapacityThresholds::nvme();
        assert_eq!(thresholds.health(86), PoolHealth::Critical);

        // No same-class sibling.
        let same_class_healthy: Vec<&AffinityPool> = pools
            .iter()
            .filter(|p| p.device_class == DeviceClass::NvmeSsd && p.name != "fast-nvme")
            .collect();

        assert!(same_class_healthy.is_empty());

        // This condition maps to ENOSPC (PoolFull error).
        let err = ChunkError::PoolFull("fast-nvme: no same-class sibling available".into());
        assert!(matches!(err, ChunkError::PoolFull(_)));
    }
}
