//! PyO3 bindings for the Kiseki native client.
//!
//! Enabled with `--features python`. Exposes cache, staging, and
//! advisory APIs to Python workloads.
//!
//! Build: `maturin develop --features python`
//!
//! Usage:
//! ```python
//! import kiseki
//! client = kiseki.Client(cache_mode="organic", cache_dir="/tmp/kiseki")
//! client.stage("/training/imagenet")
//! # ... workload reads via FUSE or native API ...
//! client.release("/training/imagenet")
//! stats = client.cache_stats()
//! client.close()
//! ```

use pyo3::prelude::*;

use crate::advisory::ClientAdvisory;
use crate::cache::{CacheConfig, CacheManager, CacheMode};
use crate::staging::{StagingConfig, StagingManager};

/// Python-facing Kiseki client.
#[pyclass]
pub struct Client {
    cache: CacheManager,
    staging: StagingManager,
    advisory: ClientAdvisory,
}

#[pymethods]
impl Client {
    /// Create a new client.
    ///
    /// Args:
    ///     cache_mode: "pinned", "organic", or "bypass" (default: "organic")
    ///     cache_dir: path for L2 NVMe cache (default: "/tmp/kiseki-cache")
    ///     cache_l2_max: max L2 bytes (default: 50 GB)
    ///     meta_ttl_ms: metadata TTL in ms (default: 5000)
    #[new]
    #[pyo3(signature = (cache_mode="organic", cache_dir="/tmp/kiseki-cache", cache_l2_max=None, meta_ttl_ms=5000))]
    fn new(
        cache_mode: &str,
        cache_dir: &str,
        cache_l2_max: Option<u64>,
        meta_ttl_ms: u64,
    ) -> PyResult<Self> {
        let mode = match cache_mode {
            "pinned" => CacheMode::Pinned,
            "bypass" => CacheMode::Bypass,
            _ => CacheMode::Organic,
        };
        let config = CacheConfig {
            mode,
            cache_dir: std::path::PathBuf::from(cache_dir),
            max_cache_bytes: cache_l2_max.unwrap_or(50 * 1024 * 1024 * 1024),
            metadata_ttl: std::time::Duration::from_millis(meta_ttl_ms),
            ..CacheConfig::default()
        };
        let cache = CacheManager::new(&config)
            .map_err(|e| pyo3::exceptions::PyIOError::new_err(e.to_string()))?;
        let staging = StagingManager::new(None, StagingConfig::default());
        let advisory = ClientAdvisory::new();

        Ok(Self {
            cache,
            staging,
            advisory,
        })
    }

    /// Stage a dataset into the local cache.
    fn stage(&mut self, namespace_path: &str) -> PyResult<()> {
        // Record staging intent. Actual chunk fetching requires
        // canonical access — deferred to full integration.
        self.staging
            .record_staged(namespace_path.to_owned(), &[], 0);
        Ok(())
    }

    /// Check staging status.
    fn stage_status(&self) -> Vec<String> {
        self.staging
            .list()
            .iter()
            .map(|d| d.namespace_path.clone())
            .collect()
    }

    /// Release a staged dataset.
    fn release(&mut self, namespace_path: &str) {
        let chunks = self.staging.release(namespace_path);
        for chunk_id in &chunks {
            self.cache.invalidate_chunk(chunk_id);
        }
    }

    /// Release all staged datasets.
    fn release_all(&mut self) {
        let chunks = self.staging.release_all();
        for chunk_id in &chunks {
            self.cache.invalidate_chunk(chunk_id);
        }
    }

    /// Get cache statistics.
    fn cache_stats(&self) -> CacheStatsView {
        let s = self.cache.stats();
        CacheStatsView {
            l1_hits: s.l1_hits,
            l2_hits: s.l2_hits,
            misses: s.misses,
            l1_bytes: s.l1_bytes,
            l2_bytes: s.l2_bytes,
            wipes: s.wipes,
        }
    }

    /// Get current cache mode.
    fn cache_mode(&self) -> &'static str {
        match self.cache.mode() {
            CacheMode::Pinned => "pinned",
            CacheMode::Organic => "organic",
            CacheMode::Bypass => "bypass",
        }
    }

    /// Declare a workflow for advisory integration.
    fn declare_workflow(&mut self) -> u128 {
        let session = self.advisory.declare_workflow();
        session.workflow_id()
    }

    /// End a workflow.
    fn end_workflow(&mut self, workflow_id: u128) {
        self.advisory.end_workflow(workflow_id);
    }

    /// Wipe the cache.
    fn wipe(&mut self) {
        self.cache.wipe();
    }

    /// Close the client (wipe cache).
    fn close(&mut self) {
        self.cache.wipe();
    }
}

/// Python-visible cache statistics.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct CacheStatsView {
    #[pyo3(get)]
    l1_hits: u64,
    #[pyo3(get)]
    l2_hits: u64,
    #[pyo3(get)]
    misses: u64,
    #[pyo3(get)]
    l1_bytes: u64,
    #[pyo3(get)]
    l2_bytes: u64,
    #[pyo3(get)]
    wipes: u64,
}

#[pymethods]
impl CacheStatsView {
    fn __repr__(&self) -> String {
        format!(
            "CacheStats(l1_hits={}, l2_hits={}, misses={}, l1_bytes={}, l2_bytes={}, wipes={})",
            self.l1_hits, self.l2_hits, self.misses, self.l1_bytes, self.l2_bytes, self.wipes
        )
    }
}

/// Python module definition.
#[pymodule]
fn kiseki(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Client>()?;
    m.add_class::<CacheStatsView>()?;
    Ok(())
}
