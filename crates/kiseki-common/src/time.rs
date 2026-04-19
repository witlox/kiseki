//! Dual clock model: Hybrid Logical Clock for ordering (I-T5, I-T7),
//! wall time for duration-based policies, per-node `ClockQuality` for
//! drift detection (I-T6).

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;

/// The HLC address space is exhausted: both the physical component
/// (`u64::MAX`) and the logical counter (`u32::MAX`) are saturated.
/// No strictly-greater timestamp can be produced. This condition is
/// unreachable in practice (`physical_ms` = `u64::MAX` corresponds to
/// year ~584,942,417 CE) but must be handled to preserve the
/// monotonicity contract (I-T5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlcExhausted;

/// Hybrid Logical Clock — authoritative for ordering and causality.
///
/// Combines a physical-time component (ms since Unix epoch) with a
/// logical counter and the producing node's identifier. Syncs across
/// nodes via the Lamport merge rule implemented by [`HybridLogicalClock::merge`].
///
/// Spec: I-T5, I-T7, `ubiquitous-language.md#Time`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct HybridLogicalClock {
    /// Physical time component, milliseconds since the Unix epoch.
    pub physical_ms: u64,
    /// Logical counter — increments when the physical component does not
    /// advance between events on the same node, or after a merge.
    pub logical: u32,
    /// Node that produced this timestamp. Used as the final tie-breaker
    /// so that two clocks produced on different nodes in the same
    /// physical millisecond with the same logical counter still totally
    /// order.
    pub node_id: NodeId,
}

impl HybridLogicalClock {
    /// The zero clock. Useful as a starting point on a fresh node.
    #[must_use]
    pub const fn zero(node_id: NodeId) -> Self {
        Self {
            physical_ms: 0,
            logical: 0,
            node_id,
        }
    }

    /// Advance the local clock given the current wall-clock reading.
    ///
    /// If the wall-clock reading is strictly greater than `self.physical_ms`,
    /// the physical component advances and the logical counter resets to
    /// zero. Otherwise the logical counter is incremented. If the logical
    /// counter overflows, the physical component is pushed forward by 1 ms
    /// and the logical counter resets — preserving strict monotonicity.
    ///
    /// Returns [`HlcExhausted`] if both components are at their maximum
    /// values and no strictly-greater timestamp can be produced.
    ///
    /// Spec: I-T5, §HLC.
    pub fn tick(mut self, now_physical_ms: u64) -> Result<Self, HlcExhausted> {
        if now_physical_ms > self.physical_ms {
            self.physical_ms = now_physical_ms;
            self.logical = 0;
        } else if let Some(next) = self.logical.checked_add(1) {
            self.logical = next;
        } else {
            // Logical counter saturated — try to advance physical.
            match self.physical_ms.checked_add(1) {
                Some(next_phys) => {
                    self.physical_ms = next_phys;
                    self.logical = 0;
                }
                None => return Err(HlcExhausted),
            }
        }
        Ok(self)
    }

    /// Merge a received remote HLC into the local clock, given the
    /// current local wall-clock reading. Implements the HLC/Lamport rule:
    ///
    /// ```text
    /// phys'  = max(local.phys, remote.phys, now)
    /// logic' = if phys' == local.phys  == remote.phys: max(local.log, remote.log) + 1
    ///          if phys' == local.phys  != remote.phys: local.log  + 1
    ///          if phys' == remote.phys != local.phys:  remote.log + 1
    ///          otherwise (phys' == now > both):        0
    /// ```
    ///
    /// Returns [`HlcExhausted`] if both components are at their maximum
    /// values and no strictly-greater timestamp can be produced.
    ///
    /// Spec: `ubiquitous-language.md#HLC`, I-T5.
    pub fn merge(self, remote: Self, now_physical_ms: u64) -> Result<Self, HlcExhausted> {
        let local_phys = self.physical_ms;
        let remote_phys = remote.physical_ms;
        let phys_prime = local_phys.max(remote_phys).max(now_physical_ms);

        let base_logical = if phys_prime == local_phys && phys_prime == remote_phys {
            self.logical.max(remote.logical)
        } else if phys_prime == local_phys {
            self.logical
        } else if phys_prime == remote_phys {
            remote.logical
        } else {
            // now strictly dominates both inputs — reset logical.
            return Ok(Self {
                physical_ms: phys_prime,
                logical: 0,
                node_id: self.node_id,
            });
        };

        match base_logical.checked_add(1) {
            Some(logical) => Ok(Self {
                physical_ms: phys_prime,
                logical,
                node_id: self.node_id,
            }),
            None => {
                // Logical counter saturated — try to advance physical.
                match phys_prime.checked_add(1) {
                    Some(next_phys) => Ok(Self {
                        physical_ms: next_phys,
                        logical: 0,
                        node_id: self.node_id,
                    }),
                    None => Err(HlcExhausted),
                }
            }
        }
    }
}

/// Induced total order: physical first, then logical, then node id.
/// Ties on (physical, logical) across different nodes are broken by
/// `node_id` so the order is total even across the cluster.
impl Ord for HybridLogicalClock {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.physical_ms
            .cmp(&other.physical_ms)
            .then_with(|| self.logical.cmp(&other.logical))
            .then_with(|| self.node_id.0.cmp(&other.node_id.0))
    }
}

impl PartialOrd for HybridLogicalClock {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Wall clock — authoritative only for duration-based policies (retention
/// TTLs, staleness bounds, compliance deadlines, audit timestamps). Never
/// used for correctness decisions.
///
/// Spec: I-T5, `ubiquitous-language.md#WallClock`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WallTime {
    /// Milliseconds since the Unix epoch in UTC.
    pub millis_since_epoch: u64,
    /// IANA timezone name the wall-clock reading is reported in.
    pub timezone: String,
}

/// Self-reported clock quality per node. Unsync nodes are flagged —
/// staleness bounds involving their timestamps are unreliable.
///
/// Spec: I-T6.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ClockQuality {
    /// NTP-synchronized.
    Ntp,
    /// PTP-synchronized.
    Ptp,
    /// GPS-synchronized.
    Gps,
    /// No trusted time source.
    Unsync,
}

/// The triple attached to every delta and every event.
///
/// `hlc` provides ordering and causality, `wall` provides duration-based
/// policy values, `quality` qualifies how much trust the receiver may
/// place in the `wall` component.
///
/// Spec: `ubiquitous-language.md#DeltaTimestamp`, I-T5, I-T6, I-T7.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeltaTimestamp {
    /// Ordering clock (authoritative for causality).
    pub hlc: HybridLogicalClock,
    /// Wall clock (authoritative for durations only).
    pub wall: WallTime,
    /// Clock quality reported by the node that produced this timestamp.
    pub quality: ClockQuality,
}
