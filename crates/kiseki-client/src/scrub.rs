//! Cache scrub service — cleans orphaned L2 pools (ADR-031 §9).
//!
//! Run on node boot and periodically (every 60s) to wipe pools from
//! crashed client processes. Orphaned pools are detected via flock:
//! if `pool.lock` can be acquired (no live holder), the pool is
//! orphaned and wiped with zeroize.
//!
//! Usage:
//! ```text
//! kiseki-cache-scrub [--cache-dir /path] [--once]
//! ```

use std::path::Path;

use crate::staging::find_orphaned_pools;

/// Scrub a single tenant's cache directory for orphaned pools.
///
/// Returns the number of pools cleaned.
pub fn scrub_tenant(cache_dir: &Path, tenant_id: &str) -> usize {
    let orphans = find_orphaned_pools(cache_dir, tenant_id);
    let count = orphans.len();

    for pool_dir in &orphans {
        tracing::info!(
            pool = %pool_dir.display(),
            tenant = tenant_id,
            "scrubbing orphaned cache pool"
        );
        wipe_pool(pool_dir);
    }

    count
}

/// Scrub all tenants under the cache directory.
///
/// Returns the total number of pools cleaned.
#[must_use]
pub fn scrub_all(cache_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return 0;
    };

    let mut total = 0;
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            let tenant_id = entry.file_name().to_string_lossy().into_owned();
            total += scrub_tenant(cache_dir, &tenant_id);
        }
    }

    total
}

/// Wipe a pool directory: zeroize all chunk files, then delete.
fn wipe_pool(pool_dir: &Path) {
    let chunks_dir = pool_dir.join("chunks");
    if chunks_dir.exists() {
        wipe_directory_recursive(&chunks_dir);
    }

    // Remove the rest of the pool directory.
    let _ = std::fs::remove_dir_all(pool_dir);
}

/// Recursively zeroize and delete all files in a directory.
fn wipe_directory_recursive(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            wipe_directory_recursive(&p);
            let _ = std::fs::remove_dir(&p);
        } else {
            // Overwrite with zeros before unlinking (I-CC2).
            if let Ok(meta) = std::fs::metadata(&p) {
                #[allow(clippy::cast_possible_truncation)]
                let zeros = vec![0u8; meta.len() as usize];
                let _ = std::fs::write(&p, &zeros);
            }
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_empty_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(scrub_all(dir.path()), 0);
    }

    #[test]
    fn scrub_nonexistent_dir_returns_zero() {
        assert_eq!(scrub_all(Path::new("/nonexistent/kiseki-cache")), 0);
    }

    #[test]
    fn scrub_detects_orphaned_pool() {
        let dir = tempfile::tempdir().unwrap();
        let tenant_dir = dir.path().join("tenant-abc");
        let pool_dir = tenant_dir.join("pool-123");
        std::fs::create_dir_all(pool_dir.join("chunks")).unwrap();

        // Write a fake chunk file.
        std::fs::write(pool_dir.join("chunks").join("test.dat"), b"secret data").unwrap();

        // Write pool.lock but don't hold flock — simulates crash.
        std::fs::write(pool_dir.join("pool.lock"), b"").unwrap();

        let cleaned = scrub_tenant(dir.path(), "tenant-abc");
        assert_eq!(cleaned, 1);
        assert!(!pool_dir.exists(), "orphaned pool should be deleted");
    }

    #[test]
    fn scrub_all_cleans_multiple_tenants() {
        let dir = tempfile::tempdir().unwrap();

        // Create orphaned pools for two tenants.
        for tenant in &["tenant-1", "tenant-2"] {
            let pool_dir = dir.path().join(tenant).join("pool-orphan");
            std::fs::create_dir_all(pool_dir.join("chunks")).unwrap();
            std::fs::write(pool_dir.join("pool.lock"), b"").unwrap();
            std::fs::write(pool_dir.join("chunks").join("data"), b"plaintext").unwrap();
        }

        let cleaned = scrub_all(dir.path());
        assert_eq!(cleaned, 2);
    }
}
