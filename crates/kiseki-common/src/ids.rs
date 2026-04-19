//! Identifier newtypes.
//!
//! Types match `specs/architecture/data-models/common.rs` exactly and are
//! referenced from `specs/ubiquitous-language.md`. Each identifier is a
//! newtype rather than a raw primitive so the type system prevents
//! cross-identifier mixing at call sites.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque node identifier within the cluster. Raw `u64` so HLC can use
/// it as a final tiebreaker (see `time::HybridLogicalClock`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// Tenant organization identifier — the isolation domain for keys,
/// quotas, and data (I-T1, I-T3).
///
/// Spec: `ubiquitous-language.md#Organization`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct OrgId(pub uuid::Uuid);

/// Optional project within an organization.
///
/// Spec: `ubiquitous-language.md#Project`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProjectId(pub uuid::Uuid);

/// Runtime unit within a tenant.
///
/// Spec: `ubiquitous-language.md#Workload`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorkloadId(pub uuid::Uuid);

/// Smallest unit of totally-ordered deltas, backed by one Raft group.
///
/// Spec: `ubiquitous-language.md#Shard`, I-L1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ShardId(pub uuid::Uuid);

/// Content-addressed chunk identifier. 32 raw bytes — `sha256(plaintext)`
/// for cross-tenant dedup tenants, `HMAC(plaintext, tenant_key)` for
/// tenant-isolated dedup (I-K10, I-X2).
///
/// `Debug` prints a short hex prefix; `Display` prints the full hex
/// form. Never embed a `ChunkId` in an error message together with its
/// plaintext — the plaintext would leak the dedup-resistant property of
/// the HMAC variant (I-K8).
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ChunkId(pub [u8; 32]);

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form — enough to distinguish in logs, never enough to
        // reconstruct plaintext or defeat the dedup policy.
        let prefix = &self.0[..4];
        write!(f, "ChunkId(")?;
        for byte in prefix {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "…)")
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Composition identifier.
///
/// Spec: `ubiquitous-language.md#Composition`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CompositionId(pub uuid::Uuid);

/// Namespace identifier (always tenant-scoped — the "tenant namespace"
/// synonym is retired in favour of this, per ubiquitous-language).
///
/// Spec: `ubiquitous-language.md#Namespace`, I-X1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NamespaceId(pub uuid::Uuid);

/// View identifier.
///
/// Spec: `ubiquitous-language.md#View`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ViewId(pub uuid::Uuid);

/// Raft-assigned sequence number within a shard. Monotonic, gap-free,
/// total order within the shard (I-L1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SequenceNumber(pub u64);

impl SequenceNumber {
    /// Return the next sequence number, or `None` if `u64::MAX` is reached.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}
