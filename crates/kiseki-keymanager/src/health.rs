//! Key manager health reporting.

/// Health status of the key manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyManagerStatus {
    /// Healthy — accepting requests.
    Healthy,
    /// Initializing — loading keys, not ready yet.
    Initializing,
    /// Degraded — operating but with reduced redundancy.
    Degraded,
    /// Unavailable — quorum lost, cannot serve requests.
    Unavailable,
}

/// Health report from the key manager.
#[derive(Clone, Debug)]
pub struct KeyManagerHealth {
    /// Current status.
    pub status: KeyManagerStatus,
    /// Number of epochs stored.
    pub epoch_count: usize,
    /// Current (latest) epoch number, if available.
    pub current_epoch: Option<u64>,
}
