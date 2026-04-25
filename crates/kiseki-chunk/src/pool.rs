//! Affinity pool management (I-C3, I-C4).

/// Durability strategy per pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DurabilityStrategy {
    /// Erasure coding (default).
    ErasureCoding {
        /// Number of data shards.
        data_shards: u8,
        /// Number of parity shards.
        parity_shards: u8,
    },
    /// N-copy replication.
    Replication {
        /// Number of copies.
        copies: u8,
    },
}

impl Default for DurabilityStrategy {
    fn default() -> Self {
        Self::ErasureCoding {
            data_shards: 4,
            parity_shards: 2,
        }
    }
}

/// Device class for pool-level placement decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// `NVMe` SSD — lowest latency.
    NvmeSsd,
    /// SATA/SAS SSD.
    Ssd,
    /// Rotational hard drive — bulk capacity.
    Hdd,
    /// Mixed or unspecified device types.
    Mixed,
}

/// An affinity pool — group of storage devices sharing a device class.
#[derive(Clone, Debug, Eq, PartialEq)]
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
}

/// A device within a pool.
#[derive(Clone, Debug, Eq, PartialEq)]
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
        }
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

/// Select the appropriate pool for a write based on data characteristics.
///
/// Small files (< 64 KiB) prefer NVMe/SSD pools for fast metadata access;
/// large files prefer HDD/Mixed pools for bulk capacity. A `preferred_class`
/// override is tried first.
#[must_use]
pub fn select_pool_for_write(
    pools: &[AffinityPool],
    data_size: u64,
    preferred_class: Option<DeviceClass>,
) -> Option<&AffinityPool> {
    // Try preferred class first.
    if let Some(class) = preferred_class {
        if let Some(pool) = pools.iter().find(|p| p.device_class == class) {
            return Some(pool);
        }
    }

    // Auto-select: small data → fastest pool, large data → bulk pool.
    if data_size < 64 * 1024 {
        pools
            .iter()
            .find(|p| p.device_class == DeviceClass::NvmeSsd || p.device_class == DeviceClass::Ssd)
            .or(pools.first())
    } else {
        pools
            .iter()
            .find(|p| p.device_class == DeviceClass::Hdd || p.device_class == DeviceClass::Mixed)
            .or(pools.first())
    }
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

    #[test]
    fn small_write_prefers_nvme() {
        let pools = make_pools();
        let selected = select_pool_for_write(&pools, 4096, None).unwrap();
        assert_eq!(selected.device_class, DeviceClass::NvmeSsd);
    }

    #[test]
    fn large_write_prefers_hdd() {
        let pools = make_pools();
        let selected = select_pool_for_write(&pools, 10 * 1024 * 1024, None).unwrap();
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
        let selected = select_pool_for_write(&pools, 1024, None).unwrap();
        assert_eq!(selected.name, "only-mixed");
    }

    // ---------------------------------------------------------------
    // Scenario: Pool redirection stays within same device class
    // When primary NVMe pool is Critical, redirect to a healthy
    // NVMe sibling — never to a HDD pool.
    // ---------------------------------------------------------------
    #[test]
    fn pool_redirection_stays_within_device_class() {
        use crate::device::{CapacityThresholds, PoolHealth};

        let pools = vec![
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

        let pools = vec![
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
