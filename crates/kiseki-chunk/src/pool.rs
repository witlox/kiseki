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
}
