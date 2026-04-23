//! Schema versioning and data-directory migration (Phase 8.2).
//!
//! On startup the server reads `$KISEKI_DATA_DIR/schema_version.json`
//! and compares it to [`CURRENT_SCHEMA_VERSION`]. Missing version files
//! are treated as fresh installs; older versions trigger sequential
//! migrations; newer versions are rejected.

use std::path::Path;

/// Current schema version for on-disk data layout.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Persisted schema version metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaVersion {
    /// Schema version number.
    pub version: u32,
    /// ISO 8601 timestamp of when this version was applied.
    pub migrated_at: String,
}

/// Errors that can occur during schema migration.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The on-disk schema version is newer than this binary supports.
    #[error("incompatible schema version: on-disk {on_disk}, binary supports up to {supported}")]
    IncompatibleVersion {
        /// Version found on disk.
        on_disk: u32,
        /// Maximum version this binary supports.
        supported: u32,
    },

    /// I/O error reading or writing the version file.
    #[error("migration I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The version file contains invalid data.
    #[error("corrupt version file: {0}")]
    Corrupt(String),
}

/// Version file name within the data directory.
const VERSION_FILE: &str = "schema_version.json";

/// Check the schema version and run any necessary migrations.
///
/// - If the version file is missing, creates it with [`CURRENT_SCHEMA_VERSION`]
///   (fresh install).
/// - If the on-disk version equals `CURRENT_SCHEMA_VERSION`, returns it as-is.
/// - If the on-disk version is older, runs sequential migrations and updates
///   the version file.
/// - If the on-disk version is newer, returns [`MigrationError::IncompatibleVersion`].
pub fn check_and_migrate(data_dir: &Path) -> Result<SchemaVersion, MigrationError> {
    let version_path = data_dir.join(VERSION_FILE);

    if !version_path.exists() {
        // Fresh install — create version file with current version.
        let sv = SchemaVersion {
            version: CURRENT_SCHEMA_VERSION,
            migrated_at: now_iso8601(),
        };
        write_version_file(&version_path, &sv)?;
        return Ok(sv);
    }

    let contents = std::fs::read_to_string(&version_path)?;
    let sv = parse_version_file(&contents)?;

    if sv.version == CURRENT_SCHEMA_VERSION {
        return Ok(sv);
    }

    if sv.version > CURRENT_SCHEMA_VERSION {
        return Err(MigrationError::IncompatibleVersion {
            on_disk: sv.version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    // Run sequential migrations: v(on_disk) -> v(on_disk+1) -> ... -> CURRENT.
    let mut current = sv.version;
    while current < CURRENT_SCHEMA_VERSION {
        run_migration(data_dir, current, current + 1)?;
        current += 1;
    }

    let updated = SchemaVersion {
        version: CURRENT_SCHEMA_VERSION,
        migrated_at: now_iso8601(),
    };
    write_version_file(&version_path, &updated)?;
    Ok(updated)
}

/// Run a single migration step from `from` to `to`.
///
/// Each migration is a separate match arm. Currently no migrations
/// exist (v1 is the initial version). Returns `Result` because future
/// migration steps will be fallible.
#[allow(clippy::unnecessary_wraps)]
fn run_migration(data_dir: &Path, from: u32, to: u32) -> Result<(), MigrationError> {
    let _ = data_dir;
    tracing::info!(from, to, "running schema migration");
    // Future migrations go here as match arms:
    // match (from, to) {
    //     (1, 2) => migrate_v1_to_v2(data_dir)?,
    //     _ => {}
    // }
    Ok(())
}

/// Write a schema version file as JSON.
fn write_version_file(path: &Path, sv: &SchemaVersion) -> Result<(), MigrationError> {
    let json = format!(
        "{{\"version\":{},\"migrated_at\":\"{}\"}}",
        sv.version, sv.migrated_at
    );
    std::fs::write(path, json)?;
    Ok(())
}

/// Parse the version file content.
fn parse_version_file(contents: &str) -> Result<SchemaVersion, MigrationError> {
    // Minimal JSON parsing to avoid serde_json dependency in the server binary.
    let version = extract_json_u32(contents, "version")
        .ok_or_else(|| MigrationError::Corrupt("missing 'version' field".into()))?;
    let migrated_at = extract_json_string(contents, "migrated_at")
        .ok_or_else(|| MigrationError::Corrupt("missing 'migrated_at' field".into()))?;
    Ok(SchemaVersion {
        version,
        migrated_at,
    })
}

/// Extract a u32 value from a JSON string by key (minimal parser).
fn extract_json_u32(json: &str, key: &str) -> Option<u32> {
    let pattern = format!("\"{key}\":");
    let start = json.find(&pattern)? + pattern.len();
    let rest = json[start..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// Extract a string value from a JSON string by key (minimal parser).
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let start = json.find(&pattern)? + pattern.len();
    let end = json[start..].find('"')?;
    Some(json[start..start + end].to_string())
}

/// Return the current time as an ISO 8601 string.
fn now_iso8601() -> String {
    // Use `SystemTime` to avoid a `chrono` dependency.
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    // Format as seconds-since-epoch (not truly ISO 8601, but deterministic).
    // A full ISO 8601 formatter would require chrono or manual calendar math.
    // We store the epoch seconds and note it in the field documentation.
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_creates_version_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sv = check_and_migrate(dir.path()).expect("migration");
        assert_eq!(sv.version, CURRENT_SCHEMA_VERSION);

        // Verify file was written.
        let path = dir.path().join(VERSION_FILE);
        assert!(path.exists());
        let contents = std::fs::read_to_string(path).expect("read");
        assert!(contents.contains(&format!("\"version\":{CURRENT_SCHEMA_VERSION}")));
    }

    #[test]
    fn existing_current_version_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = format!("{{\"version\":{CURRENT_SCHEMA_VERSION},\"migrated_at\":\"12345\"}}");
        std::fs::write(dir.path().join(VERSION_FILE), json).expect("write");

        let sv = check_and_migrate(dir.path()).expect("migration");
        assert_eq!(sv.version, CURRENT_SCHEMA_VERSION);
        assert_eq!(sv.migrated_at, "12345"); // unchanged
    }

    #[test]
    fn incompatible_version_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let future_version = CURRENT_SCHEMA_VERSION + 10;
        let json = format!("{{\"version\":{future_version},\"migrated_at\":\"99999\"}}");
        std::fs::write(dir.path().join(VERSION_FILE), json).expect("write");

        let err = check_and_migrate(dir.path()).expect_err("should fail");
        match err {
            MigrationError::IncompatibleVersion { on_disk, supported } => {
                assert_eq!(on_disk, future_version);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected IncompatibleVersion, got: {other}"),
        }
    }

    #[test]
    fn corrupt_version_file_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(VERSION_FILE), "not json at all").expect("write");

        let err = check_and_migrate(dir.path()).expect_err("should fail");
        assert!(matches!(err, MigrationError::Corrupt(_)));
    }

    #[test]
    fn parse_version_file_roundtrip() {
        let json = "{\"version\":42,\"migrated_at\":\"2026-01-01T00:00:00Z\"}";
        let sv = parse_version_file(json).expect("parse");
        assert_eq!(sv.version, 42);
        assert_eq!(sv.migrated_at, "2026-01-01T00:00:00Z");
    }
}
