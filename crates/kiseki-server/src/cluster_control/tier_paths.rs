//! ADR-049 §D8.1 — `kiseki-tier-paths.json` pointer file + I-CP-Move
//! enforcement.
//!
//! On every boot, before opening any catalog-resolved fjall store,
//! `runtime.rs` reads the pointer file at
//! `$KISEKI_DATA_DIR/kiseki-tier-paths.json` and compares each tier's
//! resolved path against the prior path. If they differ AND a
//! non-empty fjall keyspace exists at the prior path, the boot
//! refuses (I-CP-Move) — the operator must run
//! `kiseki-admin storage migrate` first.
//!
//! Acceptance criteria covered:
//!   - Q23 / N-2: pointer file is written atomically (tmp + rename),
//!     0600 permissions, corrupt JSON treated as `RefuseToOpen`
//!     (NOT first-boot).
//!   - Q31 / N-11: unit tests covering missing/corrupt/identical/
//!     mismatched-empty/mismatched-non-empty cases.
//!   - I-CP-Move: a fjall keyspace MUST NOT be opened at a new
//!     resolved path while a non-empty keyspace exists at the
//!     prior resolved path.

#![allow(dead_code)] // phase-5a wires this into runtime; until then dead

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kiseki_common::FjallStoreTier;
use serde::{Deserialize, Serialize};

/// File name relative to `KISEKI_DATA_DIR`.
pub const POINTER_FILE_NAME: &str = "kiseki-tier-paths.json";

/// Filesystem permission for the pointer file. 0600 = owner read+write
/// only (operator + the kiseki-server process; nobody else). Q23 / N-2
/// acceptance.
const POINTER_FILE_MODE: u32 = 0o600;

/// On-disk pointer file shape — maps each `FjallStoreTier::as_label()`
/// to the absolute path of the fjall keyspace that was opened on the
/// previous boot.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TierPaths {
    /// `tier_label → mount_path`. Stable keys: `small_object`,
    /// `intent_store`, `raft_log`, `composition_meta`, `chunk_meta`.
    pub paths: HashMap<String, PathBuf>,
}

impl TierPaths {
    /// Construct from an iterator of `(tier, path)` pairs. Used by
    /// phase 5a after the resolver has produced fresh paths.
    #[must_use]
    pub fn from_resolved<I>(resolved: I) -> Self
    where
        I: IntoIterator<Item = (FjallStoreTier, PathBuf)>,
    {
        let mut paths = HashMap::new();
        for (tier, path) in resolved {
            paths.insert(tier.as_label().to_owned(), path);
        }
        Self { paths }
    }

    /// Lookup the prior path for a tier; returns `None` if the
    /// pointer didn't record one (first boot, or a new tier added
    /// after the cluster's last boot).
    #[must_use]
    pub fn prior_path(&self, tier: FjallStoreTier) -> Option<&PathBuf> {
        self.paths.get(tier.as_label())
    }
}

/// Reasons a pointer-file load can fail.
#[derive(Debug, thiserror::Error)]
pub enum PointerFileError {
    #[error("ADR-049 I-CP-Move: pointer file is corrupt JSON; refuse to open fjall stores. \
             Manual recovery: delete after confirming no orphan keyspaces exist, or restore from backup. \
             Underlying error: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("ADR-049 pointer file I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Outcome of `compare_with_resolved` for one tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TierMatchOutcome {
    /// First boot OR prior pointer matches the new resolved path.
    /// Safe to open the fjall store at the resolved path.
    Ok,
    /// Prior path differs from the new resolved path, AND a
    /// non-empty fjall keyspace still exists at the prior path.
    /// I-CP-Move trips: REFUSE TO OPEN. The operator must run
    /// `kiseki-admin storage migrate` (§D8) before retrying.
    RefuseMove {
        tier: FjallStoreTier,
        prior: PathBuf,
        resolved: PathBuf,
    },
    /// Prior path differs from the new resolved path, but the
    /// prior path is empty (or the directory no longer exists).
    /// Safe to proceed — this is a clean adoption (operator ran
    /// migrate already, or the prior keyspace was reaped).
    CleanAdopt {
        tier: FjallStoreTier,
        prior: PathBuf,
        resolved: PathBuf,
    },
}

/// Load the pointer file from `$data_dir/kiseki-tier-paths.json`.
///
/// Returns:
///   - `Ok(None)` on first-boot (file does not exist).
///   - `Ok(Some(paths))` on successful load.
///   - `Err(PointerFileError::Corrupt)` if the file exists but is
///     malformed JSON. The caller MUST treat this as `RefuseToOpen`
///     and surface a structured error to the operator (Q23 / N-2):
///     a corrupt pointer is NOT first-boot.
///   - `Err(PointerFileError::Io)` for any other I/O failure.
pub fn load(data_dir: &Path) -> Result<Option<TierPaths>, PointerFileError> {
    let path = data_dir.join(POINTER_FILE_NAME);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PointerFileError::Io(e)),
    };
    let parsed: TierPaths = serde_json::from_slice(&bytes)?;
    Ok(Some(parsed))
}

/// Write the pointer file atomically: write to `kiseki-tier-paths.json.tmp`
/// in the same directory, then `rename` over the destination. The
/// `rename(2)` syscall is atomic on POSIX filesystems — readers (e.g.
/// concurrent `load()`) either see the old content or the new content,
/// never a partial write. Q23 / N-2 acceptance.
///
/// Sets `0600` permissions on the temp file before rename so the
/// final file inherits secure permissions atomically.
pub fn save(data_dir: &Path, paths: &TierPaths) -> Result<(), PointerFileError> {
    fs::create_dir_all(data_dir).map_err(PointerFileError::Io)?;
    let final_path = data_dir.join(POINTER_FILE_NAME);
    let tmp_path = data_dir.join(format!("{POINTER_FILE_NAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(paths).map_err(PointerFileError::Corrupt)?;
    fs::write(&tmp_path, bytes).map_err(PointerFileError::Io)?;
    // Set permissions on the tmp file BEFORE renaming so the final
    // file has secure permissions from the moment it appears.
    let mut perms = fs::metadata(&tmp_path)
        .map_err(PointerFileError::Io)?
        .permissions();
    perms.set_mode(POINTER_FILE_MODE);
    fs::set_permissions(&tmp_path, perms).map_err(PointerFileError::Io)?;
    fs::rename(&tmp_path, &final_path).map_err(PointerFileError::Io)?;
    Ok(())
}

/// Compare the prior pointer file's record for a tier against the
/// resolver's newly-derived path. Implements I-CP-Move (§D8.1).
///
/// `keyspace_is_non_empty` is a caller-supplied predicate that returns
/// true when the path contains a non-empty fjall keyspace (i.e.
/// opening it would surface real data). Phase 5a uses
/// `fjall::Database::open + Database::keyspace_count() > 0` —
/// abstracted here so the I-CP-Move logic is unit-testable without
/// spinning up a real fjall keyspace.
pub fn compare_tier<F: FnOnce(&Path) -> bool>(
    tier: FjallStoreTier,
    prior_paths: Option<&TierPaths>,
    resolved_path: &Path,
    keyspace_is_non_empty: F,
) -> TierMatchOutcome {
    // first-boot for this tier when no prior path recorded
    let Some(prior) = prior_paths.and_then(|p| p.prior_path(tier)) else {
        return TierMatchOutcome::Ok;
    };
    if prior == resolved_path {
        return TierMatchOutcome::Ok;
    }
    // Paths differ. Check the prior path's keyspace state.
    if keyspace_is_non_empty(prior) {
        TierMatchOutcome::RefuseMove {
            tier,
            prior: prior.clone(),
            resolved: resolved_path.to_path_buf(),
        }
    } else {
        TierMatchOutcome::CleanAdopt {
            tier,
            prior: prior.clone(),
            resolved: resolved_path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn load_returns_none_on_first_boot() {
        let dir = tmp();
        let result = load(dir.path()).expect("load ok");
        assert!(result.is_none(), "first-boot must return None");
    }

    #[test]
    fn save_then_load_round_trips_with_secure_permissions() {
        let dir = tmp();
        let mut paths = TierPaths::default();
        paths.paths.insert(
            "small_object".into(),
            PathBuf::from("/mnt/nvme0/kiseki/small-object"),
        );
        paths.paths.insert(
            "intent_store".into(),
            PathBuf::from("/mnt/nvme0/kiseki/intent-store"),
        );
        save(dir.path(), &paths).expect("save ok");

        // Verify 0600 permissions (Q23 / N-2 acceptance).
        let mode = fs::metadata(dir.path().join(POINTER_FILE_NAME))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "pointer file MUST be 0600 (owner read/write only)"
        );

        let loaded = load(dir.path()).expect("load ok").expect("file present");
        assert_eq!(loaded, paths);
    }

    #[test]
    fn load_returns_corrupt_error_on_malformed_json() {
        // Q23 / N-2: corrupt JSON MUST surface as `Corrupt`, NOT as
        // first-boot. The caller (runtime.rs) treats this as
        // RefuseToOpen.
        let dir = tmp();
        fs::write(dir.path().join(POINTER_FILE_NAME), b"{not valid json").expect("seed");
        let result = load(dir.path());
        assert!(matches!(result, Err(PointerFileError::Corrupt(_))));
    }

    #[test]
    fn save_is_atomic_under_concurrent_load_via_rename() {
        // The atomic-write contract: at NO POINT can a reader see
        // partial content (because rename(2) is atomic on POSIX).
        // We can't easily prove the negative in a single thread,
        // but we can verify the no-tmp-leftover after a successful
        // save (the tmp file must not exist).
        let dir = tmp();
        let mut paths = TierPaths::default();
        paths.paths.insert(
            "chunk_meta".into(),
            PathBuf::from("/mnt/sata0/kiseki/chunk-meta"),
        );
        save(dir.path(), &paths).expect("save ok");
        assert!(
            !dir.path().join(format!("{POINTER_FILE_NAME}.tmp")).exists(),
            ".tmp file must be removed by atomic rename"
        );
    }

    #[test]
    fn compare_tier_first_boot_for_tier_returns_ok() {
        // No prior pointer file at all → first boot for every tier.
        let outcome = compare_tier(
            FjallStoreTier::SmallObject,
            None,
            &PathBuf::from("/mnt/nvme0/kiseki/small-object"),
            |_| panic!("predicate must not be called when no prior path"),
        );
        assert_eq!(outcome, TierMatchOutcome::Ok);
    }

    #[test]
    fn compare_tier_prior_matches_resolved_returns_ok() {
        let mut prior = TierPaths::default();
        prior.paths.insert(
            "small_object".into(),
            PathBuf::from("/mnt/nvme0/kiseki/small-object"),
        );
        let outcome = compare_tier(
            FjallStoreTier::SmallObject,
            Some(&prior),
            &PathBuf::from("/mnt/nvme0/kiseki/small-object"),
            |_| panic!("predicate must not be called when paths match"),
        );
        assert_eq!(outcome, TierMatchOutcome::Ok);
    }

    #[test]
    fn compare_tier_path_differs_and_prior_non_empty_returns_refuse_move() {
        // I-CP-Move trip.
        let mut prior = TierPaths::default();
        prior.paths.insert(
            "small_object".into(),
            PathBuf::from("/mnt/sata0/kiseki/small-object"),
        );
        let outcome = compare_tier(
            FjallStoreTier::SmallObject,
            Some(&prior),
            &PathBuf::from("/mnt/nvme0/kiseki/small-object"),
            |_| true, // prior keyspace is non-empty
        );
        match outcome {
            TierMatchOutcome::RefuseMove {
                tier,
                prior,
                resolved,
            } => {
                assert_eq!(tier, FjallStoreTier::SmallObject);
                assert_eq!(prior, PathBuf::from("/mnt/sata0/kiseki/small-object"));
                assert_eq!(resolved, PathBuf::from("/mnt/nvme0/kiseki/small-object"));
            }
            other => panic!("expected RefuseMove, got {other:?}"),
        }
    }

    #[test]
    fn compare_tier_path_differs_and_prior_empty_returns_clean_adopt() {
        let mut prior = TierPaths::default();
        prior.paths.insert(
            "small_object".into(),
            PathBuf::from("/mnt/sata0/kiseki/small-object"),
        );
        let outcome = compare_tier(
            FjallStoreTier::SmallObject,
            Some(&prior),
            &PathBuf::from("/mnt/nvme0/kiseki/small-object"),
            |_| false, // prior keyspace is empty (operator ran migrate)
        );
        match outcome {
            TierMatchOutcome::CleanAdopt {
                tier,
                prior,
                resolved,
            } => {
                assert_eq!(tier, FjallStoreTier::SmallObject);
                assert_eq!(prior, PathBuf::from("/mnt/sata0/kiseki/small-object"));
                assert_eq!(resolved, PathBuf::from("/mnt/nvme0/kiseki/small-object"));
            }
            other => panic!("expected CleanAdopt, got {other:?}"),
        }
    }

    #[test]
    fn from_resolved_indexes_by_tier_label() {
        let paths = TierPaths::from_resolved(vec![
            (
                FjallStoreTier::SmallObject,
                PathBuf::from("/mnt/nvme0/kiseki/small-object"),
            ),
            (
                FjallStoreTier::IntentStore,
                PathBuf::from("/mnt/nvme0/kiseki/intent-store"),
            ),
        ]);
        assert_eq!(
            paths.prior_path(FjallStoreTier::SmallObject),
            Some(&PathBuf::from("/mnt/nvme0/kiseki/small-object"))
        );
        assert_eq!(paths.prior_path(FjallStoreTier::ChunkMeta), None);
    }
}
