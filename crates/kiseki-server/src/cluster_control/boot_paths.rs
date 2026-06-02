//! ADR-049 runtime tier-path resolution.
//!
//! Bridges the pointer file (`kiseki-tier-paths.json` produced by
//! [`super::phase5_boot`]) to the four fjall keyspace opens that
//! happen during `runtime.rs` startup:
//!
//! - `SmallObject` — the gateway's inline / multi-node-inline store
//!   (ADR-022 rev-5, #129)
//! - `IntentStore` — per-shard ADR-047 leaderless intent log
//! - `CompositionMeta` — gateway compositions persistence
//! - `ChunkMeta` — chunk store envelope/extent metadata
//!
//! (`RaftLog` is excluded from the resolver because the control-plane
//! Raft needs a deterministic path *before* phase 5a can run; it stays
//! at `<data_dir>/raft/...`.)
//!
//! ## Contract
//!
//! 1. On boot, [`BootTierPaths::load`] reads the pointer file once.
//!    `Ok(BootTierPaths)` in both first-boot (no file) and Nth-boot
//!    (pointer present) cases.
//! 2. Each consumer asks for its tier path via
//!    [`BootTierPaths::small_object`] / `intent_store_base` /
//!    `composition_meta` / `chunk_meta`. Each method falls back to
//!    `<data_dir>/<convention>` when the pointer didn't supply a
//!    value for that tier (first boot, or new tier added after the
//!    pointer was last written).
//! 3. When the pointer supplies a value, it is treated as the
//!    **fully-resolved per-tier path** (the resolver's `chosen_mount`,
//!    which is `<mount>/kiseki/<tier_dir>` by the time it lands here)
//!    and the runtime opens fjall *at exactly that path*. No further
//!    subdirectory join — phase 5a already laid out per-tier directories
//!    cleanly under each mount.
//! 4. After all stores open, [`super::phase5_boot::run`] rewrites
//!    the pointer file with whatever the *new* resolver output is —
//!    if policy or inventory changed, that's what next boot will
//!    read. The `I-CP-Move` guard inside `phase5_boot` returns
//!    `PathVersionMismatch` when prior pointer and new resolver
//!    diverge AND the prior keyspace is non-empty, forcing the
//!    operator to run `kiseki-admin storage migrate` between boots.
//!
//! ## What lands today
//!
//! Phase 5a (already on main) computes resolved paths and writes the
//! pointer. This module adds the *runtime read* — fjall stores open
//! at the pointer-recorded paths on every boot after the first. It's
//! the link that turns ADR-049 from documentation into runtime
//! behaviour.

use std::path::{Path, PathBuf};

use kiseki_common::FjallStoreTier;

use super::tier_paths::{self, PointerFileError, TierPaths};

/// Resolved tier paths the runtime should open at, derived from the
/// pointer file written by the *previous* boot's `phase5_boot::run`.
///
/// First-boot or missing-entry behaviour: each accessor falls back
/// to a deterministic `<data_dir>/<convention>` path. That keeps
/// single-host / dev / CI deployments working without any operator
/// setup AND lets phase 5a record the same path it would have
/// computed anyway.
#[derive(Clone, Debug, Default)]
pub struct BootTierPaths {
    inner: Option<TierPaths>,
}

impl BootTierPaths {
    /// Load the pointer file. Returns `Ok(default)` when the file
    /// doesn't exist (first boot); returns `Err` only when the file
    /// exists but is corrupt or unreadable — in that case the
    /// runtime should refuse to start (matches the `RefuseToOpen`
    /// contract in ADR-049 Q23 / N-2).
    pub fn load(data_dir: &Path) -> Result<Self, PointerFileError> {
        let inner = tier_paths::load(data_dir)?;
        Ok(Self { inner })
    }

    /// Path for the `SmallObject` tier — the gateway's inline /
    /// `inline_payloads` `SmallObjectStore` (ADR-022 rev-5, #129).
    ///
    /// Convention: pointer value used verbatim (the resolver already
    /// appended `kiseki/small-object/` to the chosen mount); else
    /// `<data_dir>/small/objects/` (the pre-ADR-049 path).
    #[must_use]
    pub fn small_object(&self, data_dir: &Path) -> PathBuf {
        self.resolved_tier(FjallStoreTier::SmallObject)
            .unwrap_or_else(|| data_dir.join("small").join("objects"))
    }

    /// Base directory under which per-shard `IntentStore` keyspaces
    /// live. The shard appends `<shard_id>/intents/` to this.
    ///
    /// Convention: pointer value used verbatim (the resolver already
    /// appended `kiseki/intent-store/` to the chosen mount); else
    /// `<data_dir>` (the pre-ADR-049 path where the shard appended
    /// `<shard_id>/intents/` directly to `data_dir`).
    #[must_use]
    pub fn intent_store_base(&self, data_dir: &Path) -> PathBuf {
        self.resolved_tier(FjallStoreTier::IntentStore)
            .unwrap_or_else(|| data_dir.to_path_buf())
    }

    /// Path for the `CompositionMeta` tier — the gateway's
    /// `CompositionStore` fjall keyspace.
    ///
    /// Convention: pointer value used verbatim; else
    /// `<data_dir>/metadata/compositions/` (pre-ADR-049 path).
    #[must_use]
    pub fn composition_meta(&self, data_dir: &Path) -> PathBuf {
        self.resolved_tier(FjallStoreTier::CompositionMeta)
            .unwrap_or_else(|| data_dir.join("metadata").join("compositions"))
    }

    /// Path for the `ChunkMeta` tier — the chunk store's envelope/
    /// extent metadata (separate from raw chunk data which lives on
    /// `KISEKI_RAW_DEVICES`).
    ///
    /// Convention: pointer value used verbatim; else
    /// `<data_dir>/chunks/meta/` (pre-ADR-049 path).
    #[must_use]
    pub fn chunk_meta(&self, data_dir: &Path) -> PathBuf {
        self.resolved_tier(FjallStoreTier::ChunkMeta)
            .unwrap_or_else(|| data_dir.join("chunks").join("meta"))
    }

    fn resolved_tier(&self, tier: FjallStoreTier) -> Option<PathBuf> {
        self.inner.as_ref()?.prior_path(tier).cloned()
    }

    /// True when the pointer file is present and supplies at least
    /// one tier. Used by runtime.rs to decide whether to log "boot
    /// using pointer-resolved paths" vs "boot using legacy
    /// `data_dir` paths".
    #[must_use]
    pub fn has_resolved(&self) -> bool {
        self.inner.as_ref().is_some_and(|p| !p.paths.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn paths_with(entries: &[(FjallStoreTier, &str)]) -> TierPaths {
        let mut paths = HashMap::new();
        for (tier, p) in entries {
            paths.insert(tier.as_label().to_owned(), PathBuf::from(p));
        }
        TierPaths { paths }
    }

    #[test]
    fn falls_back_to_data_dir_when_pointer_absent() {
        let boot = BootTierPaths { inner: None };
        let data_dir = Path::new("/var/lib/kiseki");
        assert_eq!(
            boot.small_object(data_dir),
            PathBuf::from("/var/lib/kiseki/small/objects"),
        );
        assert_eq!(
            boot.intent_store_base(data_dir),
            PathBuf::from("/var/lib/kiseki"),
        );
        assert_eq!(
            boot.composition_meta(data_dir),
            PathBuf::from("/var/lib/kiseki/metadata/compositions"),
        );
        assert_eq!(
            boot.chunk_meta(data_dir),
            PathBuf::from("/var/lib/kiseki/chunks/meta"),
        );
        assert!(!boot.has_resolved());
    }

    #[test]
    fn uses_pointer_value_verbatim() {
        // The pointer file stores `resolver.chosen_mount` which already
        // includes `kiseki/<tier-dir>` — BootTierPaths must use it
        // verbatim (no further subdir join) or the actual open path
        // would be `<...>/kiseki/<tier>/kiseki/<tier>`.
        let inner = paths_with(&[
            (
                FjallStoreTier::SmallObject,
                "/mnt/kiseki-meta/kiseki/small-object",
            ),
            (
                FjallStoreTier::IntentStore,
                "/mnt/kiseki-meta/kiseki/intent-store",
            ),
            (
                FjallStoreTier::CompositionMeta,
                "/mnt/kiseki-meta/kiseki/composition-meta",
            ),
            (
                FjallStoreTier::ChunkMeta,
                "/mnt/kiseki-meta/kiseki/chunk-meta",
            ),
        ]);
        let boot = BootTierPaths { inner: Some(inner) };
        let data_dir = Path::new("/var/lib/kiseki");
        assert_eq!(
            boot.small_object(data_dir),
            PathBuf::from("/mnt/kiseki-meta/kiseki/small-object"),
        );
        assert_eq!(
            boot.intent_store_base(data_dir),
            PathBuf::from("/mnt/kiseki-meta/kiseki/intent-store"),
        );
        assert_eq!(
            boot.composition_meta(data_dir),
            PathBuf::from("/mnt/kiseki-meta/kiseki/composition-meta"),
        );
        assert_eq!(
            boot.chunk_meta(data_dir),
            PathBuf::from("/mnt/kiseki-meta/kiseki/chunk-meta"),
        );
        assert!(boot.has_resolved());
    }

    #[test]
    fn partial_pointer_falls_back_per_missing_tier() {
        let inner = paths_with(&[(
            FjallStoreTier::SmallObject,
            "/mnt/kiseki-meta/kiseki/small-object",
        )]);
        let boot = BootTierPaths { inner: Some(inner) };
        let data_dir = Path::new("/var/lib/kiseki");
        // Pointer supplied SmallObject only — others fall back.
        assert_eq!(
            boot.small_object(data_dir),
            PathBuf::from("/mnt/kiseki-meta/kiseki/small-object"),
        );
        assert_eq!(
            boot.intent_store_base(data_dir),
            PathBuf::from("/var/lib/kiseki"),
        );
        assert_eq!(
            boot.chunk_meta(data_dir),
            PathBuf::from("/var/lib/kiseki/chunks/meta"),
        );
        // has_resolved is true even with one entry.
        assert!(boot.has_resolved());
    }
}
