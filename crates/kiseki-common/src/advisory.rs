//! Advisory surface types.
//!
//! These types live here, **not** in `kiseki-advisory`, to preserve the
//! no-cycle invariant: data-path crates (`kiseki-log`, `kiseki-chunk`,
//! `kiseki-composition`, `kiseki-view`, `kiseki-gateway-*`, `kiseki-client`)
//! accept an `Option<&OperationAdvisory>` in trait methods but MUST NOT
//! depend on `kiseki-advisory`. Instead, they depend on this crate and
//! treat every field as a preference only — a call with `None` MUST be
//! byte-for-byte equivalent in outcome to any call with advisory hints
//! attached (I-WA1, I-WA2).
//!
//! Cluster-internal identifiers (`AffinityPoolId`, shard IDs, chunk IDs,
//! device IDs, rack labels, log positions) are **forbidden** on this
//! surface (I-WA11). Pool identity is referenced via the opaque,
//! tenant-scoped [`PoolHandle`] that `kiseki-advisory` mints at
//! `DeclareWorkflow` and translates back to `AffinityPoolId` on hint
//! consumption (ADR-021 §2).
//!
//! Spec: ADR-020 (analyst), ADR-021 (architect), `specs/invariants.md`
//! I-WA1..I-WA19.

// =============================================================================
// Opaque capability references
// =============================================================================

/// Opaque capability reference for a workflow. Generated at
/// `DeclareWorkflow` with ≥128 bits of entropy, scoped to the owning
/// workload, and never reused within a workload (I-WA10).
///
/// Mere knowledge of a [`WorkflowRef`] grants no access: every advisory
/// request is separately authorized against the caller's mTLS identity
/// (I-WA3). Size is fixed at 16 bytes so the on-wire representation
/// (`x-kiseki-workflow-ref-bin` gRPC metadata entry, per ADR-021 §3) is
/// deterministic.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct WorkflowRef(pub [u8; 16]);

impl core::fmt::Debug for WorkflowRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fmt_opaque_prefix(f, "WorkflowRef", &self.0)
    }
}

/// Per-process client identifier. ≥128-bit CSPRNG draw at native-client
/// process start, never reused across processes (I-WA4). The advisory
/// registrar binds `(client_id, mTLS identity)` at first use.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ClientId(pub [u8; 16]);

impl core::fmt::Debug for ClientId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fmt_opaque_prefix(f, "ClientId", &self.0)
    }
}

/// Monotonic phase counter within a workflow. Strictly increasing;
/// `PhaseAdvance` with a non-greater value returns `PhaseNotMonotonic`
/// (I-WA13).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct PhaseId(pub u64);

/// Tenant-scoped opaque reference to an affinity pool. Minted by
/// `kiseki-advisory` at `DeclareWorkflow` time from the workload's
/// authorized pools; never reveals the cluster-internal pool identity
/// (I-WA11, I-WA19). Only the advisory runtime can translate back to
/// `AffinityPoolId`.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct PoolHandle(pub [u8; 16]);

impl core::fmt::Debug for PoolHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fmt_opaque_prefix(f, "PoolHandle", &self.0)
    }
}

// Small shared helper for Debug impls that want to print a 4-byte hex
// prefix. Keeps the elision (…) visible so logs clearly signal that the
// full handle has been truncated on purpose (I-K8).
fn fmt_opaque_prefix(
    f: &mut core::fmt::Formatter<'_>,
    label: &str,
    bytes: &[u8; 16],
) -> core::fmt::Result {
    write!(f, "{label}(")?;
    for byte in &bytes[..4] {
        write!(f, "{byte:02x}")?;
    }
    write!(f, "…)")
}

/// Descriptor returned alongside each [`PoolHandle`] in a workflow's
/// authorized pool set. The `opaque_label` is a tenant-chosen string
/// (e.g., `"fast-nvme"`) set at workload-authorization time; it is
/// meaningful to the workload operator but is **not** a cluster-internal
/// identifier, so correlation across tenants is impossible.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PoolDescriptor {
    /// Opaque tenant-scoped reference.
    pub handle: PoolHandle,
    /// Tenant-chosen label. Free-form, tenant-local meaning only.
    pub opaque_label: String,
}

// =============================================================================
// Hint enums
// =============================================================================

/// One-shot workload profile declared at `DeclareWorkflow`. Allow-listed
/// per scope (org → project → workload inheritance). A profile the
/// effective policy does not allow produces `ProfileNotAllowed` (I-WA7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WorkloadProfile {
    /// AI training run (checkpoint-heavy, bulk writes).
    AiTraining,
    /// AI inference serving (low-latency reads).
    AiInference,
    /// HPC checkpoint (burst write, bulk).
    HpcCheckpoint,
    /// Batch ETL (sequential, throughput-oriented).
    BatchEtl,
    /// Interactive (latency-sensitive, low parallelism).
    Interactive,
}

/// Access-pattern hint for a caller-owned composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AccessPattern {
    /// Sequential scan.
    Sequential,
    /// Random / point access.
    Random,
    /// Strided (e.g., dataloader with stride).
    Strided,
    /// Broadcast (many readers, one composition).
    Broadcast,
}

/// `QoS` priority class. Policy-capped per workload; a request above the
/// cap produces `PriorityNotAllowed` (I-WA14).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Priority {
    /// Bulk work (yields to everything else).
    Bulk,
    /// Batch work (default).
    Batch,
    /// Interactive work (latency-sensitive).
    Interactive,
}

/// Retention intent (GC urgency / EC scheme selection). NEVER bypasses a
/// retention hold (I-WA14, I-C2b).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RetentionIntent {
    /// Temporary, safe to GC aggressively after refcount hits zero.
    Temp,
    /// Working-set; default retention.
    Working,
    /// Final output; durable, EC-biased.
    Final,
}

/// Dedup intent. Bounded by tenant [`crate::tenancy::DedupPolicy`]
/// (I-X2); a `SharedEnsemble` hint from a tenant-isolated tenant is
/// simply ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DedupIntent {
    /// Share dedup with other workloads in the same ensemble.
    SharedEnsemble,
    /// Per-rank isolation (no cross-rank dedup).
    PerRank,
    /// Default (honour tenant-wide policy only).
    Default,
}

/// Tenant-scoped affinity preference. Data-path placement may ignore
/// this to satisfy I-C3 (placement policy), I-C4 (durability), or
/// I-C2b (retention holds) — see I-WA9.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AffinityPreference {
    /// Preferred pool handle. `None` means "no preference".
    pub preferred_pool: Option<PoolHandle>,
}

// =============================================================================
// OperationAdvisory bundle
// =============================================================================

/// Bundle passed alongside each data-path operation. Every field is
/// optional. A call with every field `None` is equivalent to a call
/// with no advisory at all (I-WA1, I-WA2). Fields identified here as
/// "unauthoritative" MUST NOT change the set of accepted outcomes for
/// the underlying data-path operation.
///
/// Spec: `specs/architecture/data-models/advisory.rs`, ADR-021 §3.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationAdvisory {
    /// Owning workflow. Lifted from the `x-kiseki-workflow-ref-bin`
    /// gRPC metadata header or from a task-local on the intra-Rust
    /// path; set to `None` if the caller did not declare a workflow.
    pub workflow_ref: Option<WorkflowRef>,
    /// Phase the workflow is currently in.
    pub phase_id: Option<PhaseId>,
    /// Access-pattern hint (unauthoritative).
    pub access_pattern: Option<AccessPattern>,
    /// Priority class (capped by policy).
    pub priority: Option<Priority>,
    /// Affinity preference (unauthoritative).
    pub affinity: Option<AffinityPreference>,
    /// Retention intent (never bypasses holds).
    pub retention_intent: Option<RetentionIntent>,
    /// Dedup intent (bounded by tenant dedup policy).
    pub dedup_intent: Option<DedupIntent>,
}

impl OperationAdvisory {
    /// The `None`-everywhere advisory — equivalent to "no advisory at
    /// all" per I-WA1.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            workflow_ref: None,
            phase_id: None,
            access_pattern: None,
            priority: None,
            affinity: None,
            retention_intent: None,
            dedup_intent: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Scenario: workflow_id is a capability reference
    // WorkflowRef is opaque 128-bit (16 bytes), not reused (I-WA10).
    // ---------------------------------------------------------------
    #[test]
    fn workflow_ref_is_opaque_128_bit() {
        let wf = WorkflowRef([0x42; 16]);
        // WorkflowRef is exactly 16 bytes (128 bits).
        assert_eq!(core::mem::size_of::<WorkflowRef>(), 16);
        assert_eq!(wf.0.len(), 16);
    }

    #[test]
    fn workflow_ref_distinct_values_not_equal() {
        let wf_a = WorkflowRef([0x01; 16]);
        let wf_b = WorkflowRef([0x02; 16]);
        assert_ne!(wf_a, wf_b);
    }

    #[test]
    fn workflow_ref_same_values_equal() {
        let wf_a = WorkflowRef([0xAB; 16]);
        let wf_b = WorkflowRef([0xAB; 16]);
        assert_eq!(wf_a, wf_b);
    }

    #[test]
    fn workflow_ref_debug_truncates() {
        // Debug output must not reveal full handle (I-K8).
        let mut bytes = [0x00u8; 16];
        bytes[0] = 0xDE;
        bytes[1] = 0xAD;
        bytes[2] = 0xBE;
        bytes[3] = 0xEF;
        let wf = WorkflowRef(bytes);
        let dbg = format!("{wf:?}");
        assert!(dbg.starts_with("WorkflowRef("));
        assert!(dbg.contains('\u{2026}')); // ellipsis character
    }

    #[test]
    fn workflow_ref_hashable_for_table_lookup() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let wf1 = WorkflowRef([0x01; 16]);
        let wf2 = WorkflowRef([0x02; 16]);
        set.insert(wf1);
        set.insert(wf2);
        assert_eq!(set.len(), 2);
        assert!(set.contains(&wf1));
    }

    #[test]
    fn operation_advisory_empty_is_none_everywhere() {
        let adv = OperationAdvisory::empty();
        assert!(adv.workflow_ref.is_none());
        assert!(adv.phase_id.is_none());
        assert!(adv.access_pattern.is_none());
        assert!(adv.priority.is_none());
        assert!(adv.affinity.is_none());
        assert!(adv.retention_intent.is_none());
        assert!(adv.dedup_intent.is_none());
    }
}
