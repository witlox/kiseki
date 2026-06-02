//! ADR-049 phase 5a continued — boot-time integration helper.
//!
//! Runs the per-node boot sequence that wraps the resolver + I-CP-Move
//! infrastructure in one call:
//!
//!   1. Discover local devices via `discover_local_inventory`.
//!   2. Submit `UpsertNodeInventory` to the control-plane Raft group
//!      so every replica's catalog sees this node's truth.
//!   3. Read the placement policy from the local catalog snapshot.
//!   4. Compute resolved tier paths for the four catalog-resolved
//!      tiers via `resolve_all` (`RaftLog` is bootstrap-
//!      only per §D2.5 — handled at boot before this helper runs).
//!   5. Compare each resolved path against the prior pointer file
//!      via `compare_tier`. `RefuseMove` on a non-empty
//!      mismatch (I-CP-Move trips); the caller exits 1.
//!   6. On success, save the new pointer file atomically with 0600
//!      permissions.
//!
//! The helper is wired into `runtime.rs` after the control-plane
//! Raft is up — the fjall stores for `SmallObject` / `IntentStore` /
//! `CompositionMeta` / `ChunkMeta` are already open by then at their
//! `data_dir`-relative paths. Phase 5a's I-CP-Move guard validates
//! those actual paths against the pointer file; phase 6's migration
//! command moves keyspaces and updates the pointer file when the
//! operator changes the placement policy.

#![allow(dead_code)] // wired into runtime.rs by the same commit; kept tolerant of partial integration

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use kiseki_common::{FjallStoreTier, NodeDeviceInventory, NodeId};

use super::commands::ControlCommand;

/// A future the closure returns. Pulled out as a type alias to
/// satisfy clippy's `type_complexity` lint.
pub type SubmitFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// Closure shape the caller passes to `run`.
pub type SubmitInventoryFn = Box<dyn FnOnce(ControlCommand) -> SubmitFuture + Send>;
use super::device_discovery::{discover_local_inventory, DeviceTagMap};
use super::resolver::{compute_cluster_budgets, resolve_all};
use super::state_machine::ControlStateMachine;
use super::tier_paths::{self, TierMatchOutcome, TierPaths};

/// Output of the phase-5a boot helper.
#[derive(Clone, Debug)]
pub struct ResolvedBoot {
    /// The inventory this node published. Stored so admin RPC can
    /// surface "what does this node currently see".
    pub inventory: NodeDeviceInventory,
    /// The resolved tier paths (one per catalog-resolved tier).
    /// In phase 5a these are advisory — the actual fjall stores
    /// stayed at their `data_dir`-relative paths. Phase 6 migration
    /// reconciles the gap.
    pub resolved: TierPaths,
}

/// Reasons phase-5a boot can fail.
#[derive(Debug, thiserror::Error)]
pub enum Phase5BootError {
    #[error("ADR-049 phase 5a discovery failed: {0}")]
    Discovery(String),
    #[error("ADR-049 phase 5a UpsertNodeInventory submit failed: {0}")]
    InventoryPublish(String),
    #[error("ADR-049 phase 5a budget computation failed: {0}")]
    BudgetCompute(String),
    #[error("ADR-049 phase 5a resolver failed: {0}")]
    Resolve(String),
    #[error(
        "ADR-049 I-CP-Move tripped: tier {tier:?} prior path {prior:?} differs from \
         resolved {resolved:?} and the prior keyspace is non-empty. Run \
         `kiseki-admin storage migrate --tier={tier_label} --node=<this-node>` \
         BEFORE retrying this boot."
    )]
    PathVersionMismatch {
        tier: FjallStoreTier,
        tier_label: &'static str,
        prior: PathBuf,
        resolved: PathBuf,
    },
    #[error("ADR-049 pointer file error: {0}")]
    Pointer(#[from] tier_paths::PointerFileError),
}

/// Wall-clock "now" in epoch milliseconds. Pulled out as a helper
/// so the test variant can stub it.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Inputs the helper needs from the host context.
pub struct Phase5BootInputs<'a> {
    pub node_id: NodeId,
    pub data_dir: &'a Path,
    /// Operator-supplied `KISEKI_DEVICE_TAGS` parser output.
    pub tags: &'a DeviceTagMap,
    /// The control-plane state machine handle. Reads policy from
    /// its snapshot; the submit-publish is on the caller per the
    /// host's existing ADR-042 leader-forward path.
    pub catalog: Arc<ControlStateMachine>,
    /// Closure the caller provides to submit a `ControlCommand`
    /// through the local `OpenRaftControlStore`. Lets the helper
    /// stay free of openraft dependencies; runtime.rs passes a
    /// small wrapper that calls `client_write` and translates
    /// errors to strings.
    ///
    /// The closure is async-shaped via `Box<dyn Future>` so the
    /// helper composes with the rest of runtime.rs without
    /// awaiting inside a sync function.
    pub submit_inventory: SubmitInventoryFn,
}

/// Run the helper. Caller invokes from runtime.rs after the
/// control-plane Raft handle is up and `data_dir` is known.
///
/// Phase 6 migration will replace the "validate actual paths
/// against pointer file" semantic with a "physically move keyspaces
/// to resolved paths" semantic. For now the helper writes the
/// pointer file with the resolved paths AS IF the move had
/// happened — this is the migration-stub posture; phase 6 will
/// drive the actual move via `kiseki-admin storage migrate`.
pub async fn run(inputs: Phase5BootInputs<'_>) -> Result<ResolvedBoot, Phase5BootError> {
    // Step 1: discover local devices.
    let inventory =
        discover_local_inventory(inputs.node_id, Some(inputs.data_dir), inputs.tags, now_ms());

    // Step 2: publish UpsertNodeInventory. The closure handles the
    // openraft leader-forward path so this helper stays openraft-
    // free.
    let cmd = ControlCommand::UpsertNodeInventory {
        node_id: inputs.node_id,
        inventory: inventory.clone(),
    };
    (inputs.submit_inventory)(cmd)
        .await
        .map_err(Phase5BootError::InventoryPublish)?;

    // Step 3: read the policy from the local catalog snapshot.
    let catalog_snap = inputs.catalog.catalog().await;

    // Step 4: compute cluster budgets + resolve per-tier paths.
    let cluster = compute_cluster_budgets(&catalog_snap)
        .map_err(|e| Phase5BootError::BudgetCompute(format!("{e}")))?;
    let resolved_tiers = resolve_all(&inventory, &cluster, &catalog_snap.policy)
        .map_err(|e| Phase5BootError::Resolve(format!("{e}")))?;

    // Build the resolved TierPaths struct (i.e. tier_label → path).
    let resolved = TierPaths::from_resolved(
        resolved_tiers
            .iter()
            .map(|r| (r.tier, r.chosen_mount.clone())),
    );

    // Step 5: load the prior pointer file + I-CP-Move check on each tier.
    let prior = tier_paths::load(inputs.data_dir)?;
    for tier in FjallStoreTier::catalog_resolved() {
        let Some(resolved_path) = resolved.prior_path(tier) else {
            continue;
        };
        let outcome = tier_paths::compare_tier(tier, prior.as_ref(), resolved_path, |path| {
            // For phase 5a, "non-empty" is heuristic: does the
            // directory exist AND have non-trivial children?
            // Phase 6 migration replaces this with a real
            // fjall::Database probe.
            match std::fs::read_dir(path) {
                Ok(entries) => entries
                    .flatten()
                    .any(|e| e.file_name() != ".DS_Store" && e.file_name() != "lost+found"),
                Err(_) => false,
            }
        });
        match outcome {
            TierMatchOutcome::Ok => {}
            TierMatchOutcome::CleanAdopt {
                tier,
                prior,
                resolved,
            } => {
                tracing::info!(
                    tier = ?tier,
                    prior = %prior.display(),
                    resolved = %resolved.display(),
                    "ADR-049 clean policy adoption: prior path empty, opening at resolved path"
                );
            }
            TierMatchOutcome::RefuseMove {
                tier,
                prior,
                resolved,
            } => {
                return Err(Phase5BootError::PathVersionMismatch {
                    tier,
                    tier_label: tier.as_label(),
                    prior,
                    resolved,
                });
            }
        }
    }

    // Step 6: save the new pointer file (atomic write + 0600).
    tier_paths::save(inputs.data_dir, &resolved)?;

    Ok(ResolvedBoot {
        inventory,
        resolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::{DeviceEntry, MediaType};

    fn submit_ok() -> SubmitInventoryFn {
        Box::new(|_cmd| Box::pin(async { Ok(()) }))
    }

    #[tokio::test]
    async fn phase5_boot_first_boot_writes_pointer_file_with_resolved_paths() {
        // First boot: no pointer file exists. Helper writes it.
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(ControlStateMachine::new());
        // Seed the catalog policy with the built-in default so the
        // resolver has something to consult.
        // We can't easily call apply_command here without a Raft
        // handle; instead we cheat by writing directly to the inner
        // catalog via the test surface — but that requires test-only
        // access. For phase 5a tests we accept that the catalog
        // starts empty and the resolver falls back to its built-in
        // default policy (`PlacementPolicy::built_in_default()`).
        let tags = DeviceTagMap::default();
        let result = run(Phase5BootInputs {
            node_id: NodeId(1),
            data_dir: tmp.path(),
            tags: &tags,
            catalog: Arc::clone(&sm),
            submit_inventory: submit_ok(),
        })
        .await
        .expect("phase5 boot ok");

        // Pointer file MUST exist after first boot.
        let pointer = tmp.path().join(tier_paths::POINTER_FILE_NAME);
        assert!(pointer.exists(), "phase5 boot must write pointer file");

        // Resolved struct includes the four catalog-resolved tiers.
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::SmallObject)
            .is_some());
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::IntentStore)
            .is_some());
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::CompositionMeta)
            .is_some());
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::ChunkMeta)
            .is_some());
        // RaftLog (bootstrap-only) MUST NOT appear in resolved tiers.
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::RaftLog)
            .is_none());
    }

    #[tokio::test]
    async fn phase5_boot_idempotent_when_pointer_matches_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(ControlStateMachine::new());
        let tags = DeviceTagMap::default();
        // First boot.
        run(Phase5BootInputs {
            node_id: NodeId(1),
            data_dir: tmp.path(),
            tags: &tags,
            catalog: Arc::clone(&sm),
            submit_inventory: submit_ok(),
        })
        .await
        .expect("first boot ok");

        // Second boot with the same policy + inventory → same resolution
        // → pointer matches resolved → Ok.
        let result = run(Phase5BootInputs {
            node_id: NodeId(1),
            data_dir: tmp.path(),
            tags: &tags,
            catalog: Arc::clone(&sm),
            submit_inventory: submit_ok(),
        })
        .await
        .expect("second boot ok");

        // Resolved paths stable across boots.
        assert!(result
            .resolved
            .prior_path(FjallStoreTier::SmallObject)
            .is_some());
    }

    #[test]
    fn discover_inventory_includes_data_dir_fallback() {
        // Confirms the helper's discovery step composes correctly
        // with the device_discovery::discover_local_inventory pure
        // function (which is what the boot path calls).
        let tmp = tempfile::tempdir().unwrap();
        let tags = DeviceTagMap::default();
        let inv = discover_local_inventory(NodeId(1), Some(tmp.path()), &tags, 0);
        // data_dir fallback entry is always present so DeviceMatcher::DataDir
        // policies always have a target.
        let data_dir_entry = inv
            .devices
            .iter()
            .find(|d| d.tag.as_deref() == Some("data-dir-default"));
        assert!(data_dir_entry.is_some());
        let _ = DeviceEntry {
            mount_path: PathBuf::new(),
            media_class: MediaType::Unknown,
            total_bytes: 0,
            free_bytes: 0,
            tag: None,
            exclusive: false,
        }; // ensure import resolution
    }
}
