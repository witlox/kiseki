//! Telemetry types for the advisory subsystem.
//!
//! Telemetry is caller-scoped (I-WA5), bucketed for k-anonymity (I-WA6),
//! and never leaks cross-tenant information.

use std::collections::HashSet;

// =============================================================================
// Locality classes (coarsely bucketed, I-WA5)
// =============================================================================

/// Locality class for telemetry — coarsely bucketed, no node/rack/device IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LocalityClass {
    /// Data served from local node.
    LocalNode,
    /// Data served from same rack.
    LocalRack,
    /// Data served from same pool.
    SamePool,
    /// Data served from remote pool.
    Remote,
    /// Degraded (reconstructed from parity or fallback).
    Degraded,
}

// =============================================================================
// Backpressure severity
// =============================================================================

/// Backpressure severity level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackpressureSeverity {
    /// Approaching budget — caller should slow down.
    Soft,
    /// At budget — caller should stop.
    Hard,
}

// =============================================================================
// Telemetry channels
// =============================================================================

/// Available telemetry subscription channels.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TelemetryChannel {
    /// Pool backpressure signals.
    Backpressure,
    /// Locality distribution for owned compositions.
    Locality,
    /// `QoS` headroom for the caller's workload.
    QosHeadroom,
    /// Own-hotspot detection for caller's compositions.
    OwnHotspot,
    /// Prefetch effectiveness.
    PrefetchEffectiveness,
}

// =============================================================================
// Stream warning kinds (bidi stream lifecycle)
// =============================================================================

/// Warning types emitted on the advisory bidi stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamWarningKind {
    /// Budget exceeded for sustained period.
    BudgetExceeded,
    /// Workflow TTL is about to expire.
    WorkflowTtlSoon,
    /// mTLS cert is near expiry.
    CertNearExpiry,
    /// Heartbeat keep-alive.
    Heartbeat,
    /// Subscription revoked due to policy narrowing.
    SubscriptionRevoked,
}

// =============================================================================
// Contention level (bucketed, I-WA5)
// =============================================================================

/// Contention level for own-hotspot telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentionLevel {
    /// Low contention.
    Low,
    /// Moderate contention.
    Moderate,
    /// Tight (high contention).
    Tight,
}

// =============================================================================
// Telemetry response (fixed-size bucketed, I-WA15)
// =============================================================================

/// K-anonymity sentinel for low-k telemetry responses.
pub const LOW_K_SENTINEL: f64 = -1.0;

/// Minimum k-anonymity threshold.
pub const K_ANONYMITY_THRESHOLD: usize = 5;

/// Telemetry response with fixed-size bucketing.
#[derive(Clone, Debug)]
pub struct TelemetryResponse {
    /// Backpressure severity (if subscribed).
    pub backpressure: Option<BackpressureSeverity>,
    /// Retry-after hint in milliseconds (for soft backpressure).
    pub retry_after_ms: Option<u64>,
    /// Locality distribution for caller's compositions.
    pub locality: Vec<LocalityClass>,
    /// Neighbour-derived aggregate saturation.
    /// Sentinel value `LOW_K_SENTINEL` when k < `K_ANONYMITY_THRESHOLD`.
    pub aggregate_saturation: f64,
    /// Number of neighbour workloads contributing (for k-anonymity check).
    pub neighbour_count: usize,
    /// Padded size bucket (fixed set of sizes to prevent size side-channel).
    pub size_bucket: usize,
}

impl TelemetryResponse {
    /// Fixed set of allowed response sizes (padded).
    const SIZE_BUCKETS: [usize; 4] = [128, 256, 512, 1024];

    /// Create a telemetry response with proper k-anonymity and size bucketing.
    #[must_use]
    pub fn new(
        backpressure: Option<BackpressureSeverity>,
        retry_after_ms: Option<u64>,
        locality: Vec<LocalityClass>,
        raw_saturation: f64,
        neighbour_count: usize,
    ) -> Self {
        let aggregate_saturation = if neighbour_count < K_ANONYMITY_THRESHOLD {
            LOW_K_SENTINEL
        } else {
            raw_saturation
        };

        // Pick the smallest bucket that fits.
        let estimated_size = 64 + locality.len() * 8;
        let size_bucket = Self::SIZE_BUCKETS
            .iter()
            .copied()
            .find(|&b| b >= estimated_size)
            .unwrap_or(*Self::SIZE_BUCKETS.last().unwrap_or(&1024));

        Self {
            backpressure,
            retry_after_ms,
            locality,
            aggregate_saturation,
            neighbour_count,
            size_bucket,
        }
    }

    /// Whether neighbour-derived fields use the sentinel value.
    #[must_use]
    pub fn is_low_k(&self) -> bool {
        self.neighbour_count < K_ANONYMITY_THRESHOLD
    }

    /// Whether the size is in the fixed bucket set.
    #[must_use]
    pub fn size_is_bucketed(&self) -> bool {
        Self::SIZE_BUCKETS.contains(&self.size_bucket)
    }
}

// =============================================================================
// Audit correlation (I-WA8)
// =============================================================================

/// Correlation fields for audit events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditCorrelation {
    /// Organization ID.
    pub org: String,
    /// Project ID.
    pub project: String,
    /// Workload ID.
    pub workload: String,
    /// Client ID.
    pub client_id: String,
    /// Workflow ID.
    pub workflow_id: String,
    /// Current phase ID.
    pub phase_id: u64,
}

// =============================================================================
// Phase summary event (I-WA13, ADR-021 section 9)
// =============================================================================

/// Phase summary audit event emitted on phase ring eviction.
#[derive(Clone, Debug)]
pub struct PhaseSummaryEvent {
    /// From phase ID.
    pub from_phase_id: u64,
    /// To phase ID.
    pub to_phase_id: u64,
    /// Log2-bucketed accepted hint count.
    pub hints_accepted_bucket: u32,
    /// Log2-bucketed rejected hint count.
    pub hints_rejected_bucket: u32,
    /// Log2-bucketed duration in milliseconds.
    pub duration_ms_bucket: u32,
}

impl PhaseSummaryEvent {
    /// Bucket a value into log2 buckets.
    #[must_use]
    pub fn log2_bucket(value: u64) -> u32 {
        if value == 0 {
            0
        } else {
            // 64 - leading_zeros gives the bit position of the highest set bit.
            // We want the bucket, so use that directly.
            64 - value.leading_zeros()
        }
    }

    /// Compute the padded wire size (fixed bucket for I-WA15).
    #[must_use]
    pub fn padded_wire_size(&self) -> usize {
        // Fixed 128-byte wire size regardless of content.
        128
    }
}

// =============================================================================
// Own-hotspot telemetry event
// =============================================================================

/// Own-hotspot telemetry event (caller's contended composition).
#[derive(Clone, Debug)]
pub struct OwnHotspotEvent {
    /// Composition ID experiencing contention.
    pub composition_id: String,
    /// Bucketed contention level.
    pub contention: ContentionLevel,
    /// Owning workload ID (for scoping validation).
    pub workload_id: String,
}

// =============================================================================
// Batched audit counter (I-WA8)
// =============================================================================

/// Batched audit counter for high-rate hint throttling.
#[derive(Clone, Debug, Default)]
pub struct BatchedAuditCounter {
    /// Accepted hints per `(workflow_id, window)`.
    accepted: u64,
    /// Throttled hints per `(workflow_id, rejection_reason, window)`.
    throttled: u64,
}

impl BatchedAuditCounter {
    /// Record an accepted hint.
    pub fn record_accepted(&mut self) {
        self.accepted += 1;
    }

    /// Record a throttled hint.
    pub fn record_throttled(&mut self) {
        self.throttled += 1;
    }

    /// Flush the counter, returning (accepted, throttled) for audit emission.
    pub fn flush(&mut self) -> (u64, u64) {
        let result = (self.accepted, self.throttled);
        self.accepted = 0;
        self.throttled = 0;
        result
    }

    /// Whether a batched audit event should be emitted (at least one event).
    #[must_use]
    pub fn should_emit(&self) -> bool {
        self.accepted > 0 || self.throttled > 0
    }
}

// =============================================================================
// Telemetry subscription tracking
// =============================================================================

/// Tracks active telemetry subscriptions for a workflow.
#[derive(Clone, Debug, Default)]
pub struct TelemetrySubscriptions {
    channels: HashSet<TelemetryChannel>,
}

impl TelemetrySubscriptions {
    /// Subscribe to a channel.
    pub fn subscribe(&mut self, channel: TelemetryChannel) {
        self.channels.insert(channel);
    }

    /// Unsubscribe from a channel.
    pub fn unsubscribe(&mut self, channel: TelemetryChannel) -> bool {
        self.channels.remove(&channel)
    }

    /// Check if subscribed to a channel.
    #[must_use]
    pub fn is_subscribed(&self, channel: &TelemetryChannel) -> bool {
        self.channels.contains(channel)
    }

    /// Get all active channels.
    #[must_use]
    pub fn active_channels(&self) -> Vec<TelemetryChannel> {
        self.channels.iter().copied().collect()
    }

    /// Revoke all subscriptions, returning the list that was revoked.
    pub fn revoke_all(&mut self) -> Vec<TelemetryChannel> {
        let revoked: Vec<_> = self.channels.iter().copied().collect();
        self.channels.clear();
        revoked
    }

    /// Revoke subscriptions for a specific pool (policy narrowing).
    /// In this stub, it revokes all subscriptions and returns them.
    pub fn revoke_for_pool(&mut self) -> Vec<TelemetryChannel> {
        self.revoke_all()
    }
}
