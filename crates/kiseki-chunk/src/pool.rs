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
}

impl AffinityPool {
    /// Create a new pool.
    #[must_use]
    pub fn new(name: &str, durability: DurabilityStrategy, capacity_bytes: u64) -> Self {
        Self {
            name: name.to_owned(),
            durability,
            capacity_bytes,
            used_bytes: 0,
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
