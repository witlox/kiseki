// Doc-style identifiers (`composition_id`, `stateid`, etc.) appear
// frequently in this module's docs; backticking each occurrence is
// noise. Same precedent as nfs_ops.rs.
#![allow(clippy::doc_markdown)]

//! pNFS DS write buffers — chunk-staging accumulator per
//! composition (ADR-038 rev 3 §D5 + §D5.1).
//!
//! The DS does not hold its own composition state; it accumulates
//! WRITE bytes into a per-composition `Vec<u8>` (zero-padded as
//! offsets grow, last-write-wins on overlap — matching `nfs_ops::
//! WriteBuffers` exactly), then drains via `GatewayOps::write` on
//! `COMMIT` (or `LAYOUTCOMMIT` / session teardown). The new
//! composition_id is recorded in a redirect table so subsequent
//! READs through the OLD MAC'd fh4 see the new bytes.
//!
//! # Why composition_id-keyed (not stateid-keyed)
//!
//! POSIX says writes are visible to other file-descriptors of the
//! same file immediately after the write returns, even before any
//! flush. Keying buffers on `stateid` would scope visibility to a
//! single OPEN; the kernel pNFS client expects file-wide visibility
//! within a session. Composition_id is the file-identity token, so
//! that's the right key.
//!
//! # Cap semantics (§D5.1)
//!
//! The cap is **per composition_id**, not per stateid as the
//! initial escalation phrased it — the file-wide buffer keying
//! makes per-stateid bookkeeping wrong. Default 256 MiB; configurable
//! via `KISEKI_PNFS_DS_BUFFER_CAP_BYTES`. Overflow returns
//! `NFS4ERR_NOSPC`.

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
use kiseki_common::locks::LockOrDie;
use prometheus::{register_int_counter_vec, register_int_gauge, IntCounterVec, IntGauge, Opts};
use std::sync::OnceLock;

/// Default per-composition buffer cap. Matches the kernel pNFS
/// client's typical dirty-data budget per file (Linux pNFS client
/// uses `wb_max_pages` ≈ host RAM / 4, but caps individual files at
/// 256 MiB by default through `nfs_pgio_max_size`). Overrideable via
/// `KISEKI_PNFS_DS_BUFFER_CAP_BYTES`.
pub const DEFAULT_BUFFER_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Read the configured per-composition buffer cap.
#[must_use]
pub fn buffer_cap_bytes() -> u64 {
    std::env::var("KISEKI_PNFS_DS_BUFFER_CAP_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUFFER_CAP_BYTES)
}

/// Outcome of a `buffer_write` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferWriteResult {
    /// Bytes were appended successfully.
    Accepted,
    /// Adding `data.len()` would exceed the per-composition cap.
    Nospc {
        /// Current buffer size.
        current: u64,
        /// Configured cap.
        cap: u64,
        /// How many additional bytes were requested.
        requested: u64,
    },
}

/// Per-composition buffer entry — the file-wide accumulator. Keys
/// `tenant_id` + `namespace_id` carry through to the `gateway.write`
/// call on flush.
#[derive(Debug, Clone)]
pub struct BufferEntry {
    /// Tenant the original composition belongs to; carried into the
    /// `WriteRequest` issued on COMMIT.
    pub tenant_id: OrgId,
    /// Namespace the original composition belongs to.
    pub namespace_id: NamespaceId,
    /// Accumulated plaintext, zero-padded across holes between writes.
    pub data: Vec<u8>,
}

/// Per-DS write buffer pool (one per `DsContext`). Internally
/// `Mutex<HashMap>`; lock pattern matches `nfs_ops::WriteBuffers`.
pub struct DsWriteBuffers {
    inner: Mutex<DsWriteBuffersInner>,
    cap_bytes: u64,
}

struct DsWriteBuffersInner {
    /// `composition_id` (the original fh4 target) → accumulated bytes.
    buffers: HashMap<CompositionId, BufferEntry>,
    /// `original_composition_id` → most-recent flushed composition_id.
    /// Read path consults this table after MAC-validating the fh4 so
    /// reads through an OLD fh4 see post-COMMIT bytes.
    redirects: HashMap<CompositionId, CompositionId>,
    /// Aggregate buffered bytes across all compositions — exposed as
    /// the `kiseki_pnfs_ds_buffer_bytes` Prometheus gauge.
    total_bytes: u64,
}

impl DsWriteBuffers {
    /// Build a new buffer pool with the configured cap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(buffer_cap_bytes())
    }

    /// Build a buffer pool with an explicit cap (test-only entry).
    #[must_use]
    pub fn with_cap(cap_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(DsWriteBuffersInner {
                buffers: HashMap::new(),
                redirects: HashMap::new(),
                total_bytes: 0,
            }),
            cap_bytes,
        }
    }

    /// Append `data` at `offset` into the buffer for `composition_id`.
    /// Allocates the entry on first write; zero-pads holes; last-write-
    /// wins on overlap (POSIX `pwrite` semantics — same as `nfs_ops::
    /// buffer_write`).
    ///
    /// Returns `Nospc` if accepting the write would push this
    /// composition's buffer past the configured cap. The buffer state
    /// is unchanged on `Nospc`.
    pub fn buffer_write(
        &self,
        composition_id: CompositionId,
        tenant_id: OrgId,
        namespace_id: NamespaceId,
        offset: u64,
        data: &[u8],
    ) -> BufferWriteResult {
        let mut g = self
            .inner
            .lock()
            .lock_or_die("pnfs_write_buffer.buffer_write");
        let entry = g.buffers.entry(composition_id).or_insert(BufferEntry {
            tenant_id,
            namespace_id,
            data: Vec::new(),
        });

        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = off.saturating_add(data.len());
        let new_len_u64 = u64::try_from(end).unwrap_or(u64::MAX);
        let cur_len_u64 = u64::try_from(entry.data.len()).unwrap_or(u64::MAX);
        let growth = new_len_u64.saturating_sub(cur_len_u64);

        // Cap check on the post-write size (only the growth contributes
        // to total_bytes — overlapping writes don't grow the buffer).
        if cur_len_u64.saturating_add(growth) > self.cap_bytes {
            ds_write_bytes_total()
                .with_label_values(&["rejected_nospc"])
                .inc_by(u64::try_from(data.len()).unwrap_or(0));
            return BufferWriteResult::Nospc {
                current: cur_len_u64,
                cap: self.cap_bytes,
                requested: u64::try_from(data.len()).unwrap_or(0),
            };
        }

        if entry.data.len() < end {
            entry.data.resize(end, 0);
        }
        entry.data[off..end].copy_from_slice(data);
        g.total_bytes = g.total_bytes.saturating_add(growth);
        ds_buffer_bytes_gauge().set(i64::try_from(g.total_bytes).unwrap_or(i64::MAX));
        ds_write_bytes_total()
            .with_label_values(&["accepted"])
            .inc_by(u64::try_from(data.len()).unwrap_or(0));
        BufferWriteResult::Accepted
    }

    /// Take the accumulated bytes for `composition_id`. Returns `None`
    /// if no buffer exists. Releases the cap allocation.
    pub fn take(&self, composition_id: CompositionId) -> Option<BufferEntry> {
        let mut g = self.inner.lock().lock_or_die("pnfs_write_buffer.take");
        let entry = g.buffers.remove(&composition_id)?;
        let bytes = u64::try_from(entry.data.len()).unwrap_or(0);
        g.total_bytes = g.total_bytes.saturating_sub(bytes);
        ds_buffer_bytes_gauge().set(i64::try_from(g.total_bytes).unwrap_or(i64::MAX));
        Some(entry)
    }

    /// Record a redirect: subsequent reads through `original` resolve
    /// to `current`. Called by `op_commit_ds` after a successful
    /// `gateway.write`.
    pub fn record_redirect(&self, original: CompositionId, current: CompositionId) {
        let mut g = self
            .inner
            .lock()
            .lock_or_die("pnfs_write_buffer.record_redirect");
        g.redirects.insert(original, current);
    }

    /// Resolve `composition_id` through the redirect table. Returns the
    /// MOST RECENT post-COMMIT composition for this fh4, or the input
    /// unchanged if no redirect is recorded.
    ///
    /// Phase 1 keeps redirects forever — bounded only by `Drop` of
    /// the `DsWriteBuffers` (one per DS process). LRU eviction would
    /// be Phase 1 follow-up if the table grows unbounded; on the GCP
    /// perf cluster shape (16k stateid LRU per DS, ~16 redirects per
    /// stateid lifetime) it's bounded at ~256k entries × 32 B ~ 8 MB.
    /// Acceptable for the perf-cluster shape; flagged for the Phase 3
    /// perf retest.
    #[must_use]
    pub fn resolve(&self, composition_id: CompositionId) -> CompositionId {
        let g = self.inner.lock().lock_or_die("pnfs_write_buffer.resolve");
        g.redirects
            .get(&composition_id)
            .copied()
            .unwrap_or(composition_id)
    }

    /// Read bytes from the buffer for `composition_id` at `offset` for
    /// `count` bytes. Returns the bytes and a flag indicating whether
    /// the read covered the requested range fully (else the caller
    /// must merge with `gateway.read` for the trailing portion).
    ///
    /// Specifically:
    /// - If the buffer covers `[offset, offset+count)` entirely,
    ///   returns `(bytes, fully_covered=true)`.
    /// - If offset is past the buffer end, returns `(empty, false)`
    ///   — caller falls back to gateway.
    /// - If offset is within the buffer but the request extends past
    ///   the buffer end, returns the prefix bytes + `fully_covered=false`
    ///   (caller merges with gateway.read for the suffix).
    pub fn read(&self, composition_id: CompositionId, offset: u64, count: u64) -> (Vec<u8>, bool) {
        let g = self.inner.lock().lock_or_die("pnfs_write_buffer.read");
        let Some(entry) = g.buffers.get(&composition_id) else {
            return (Vec::new(), false);
        };
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        let cnt = usize::try_from(count).unwrap_or(usize::MAX);
        if off >= entry.data.len() {
            return (Vec::new(), false);
        }
        let end = off.saturating_add(cnt).min(entry.data.len());
        let bytes = entry.data[off..end].to_vec();
        let fully_covered = end - off == cnt;
        (bytes, fully_covered)
    }

    /// Drop all buffers + redirects for compositions whose buffer
    /// was contributed to via this client. Called from
    /// `op_destroy_session` and `op_destroy_clientid`.
    ///
    /// Phase 1: drops ALL buffers + redirects (no per-client
    /// bookkeeping yet). Sufficient for the test patterns in
    /// `tests/e2e/test_perf_baseline.py` where there's one client
    /// per cluster lifetime. Multi-tenant DS would need
    /// per-(client_id, composition_id) tracking; flagged for
    /// follow-up if multi-client perf shows interference.
    pub fn clear_all(&self) {
        let mut g = self.inner.lock().lock_or_die("pnfs_write_buffer.clear_all");
        g.buffers.clear();
        g.redirects.clear();
        g.total_bytes = 0;
        ds_buffer_bytes_gauge().set(0);
    }

    /// Total in-flight buffered bytes — for tests + telemetry.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        let g = self
            .inner
            .lock()
            .lock_or_die("pnfs_write_buffer.total_bytes");
        g.total_bytes
    }
}

impl Default for DsWriteBuffers {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Telemetry — shared across all DS instances in a process via OnceLock.
// =============================================================================

fn ds_write_bytes_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            Opts::new(
                "kiseki_pnfs_ds_write_bytes_total",
                "Bytes accepted into / rejected from per-composition DS \
                 write buffers (ADR-038 rev 3 §D5.1). `state` ∈ \
                 {accepted, rejected_nospc}."
            ),
            &["state"],
        )
        .expect("kiseki-gateway: register pnfs_ds_write_bytes_total")
    })
}

/// COMMIT outcome counter — incremented from `op_commit_ds`.
pub fn ds_commit_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            Opts::new(
                "kiseki_pnfs_ds_commit_total",
                "DS COMMIT calls grouped by outcome. `state` ∈ \
                 {ok, no_buffer, gateway_err}."
            ),
            &["state"],
        )
        .expect("kiseki-gateway: register pnfs_ds_commit_total")
    })
}

fn ds_buffer_bytes_gauge() -> &'static IntGauge {
    static G: OnceLock<IntGauge> = OnceLock::new();
    G.get_or_init(|| {
        register_int_gauge!(
            "kiseki_pnfs_ds_buffer_bytes",
            "Total bytes currently buffered across all per-composition \
             DS write buffers."
        )
        .expect("kiseki-gateway: register pnfs_ds_buffer_bytes")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::CompositionId;
    use uuid::Uuid;

    fn cid(n: u128) -> CompositionId {
        CompositionId(Uuid::from_u128(n))
    }

    fn tn() -> (OrgId, NamespaceId) {
        (OrgId(Uuid::from_u128(1)), NamespaceId(Uuid::from_u128(2)))
    }

    #[test]
    fn buffer_write_appends_then_reads_round_trip() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();
        let r = buf.buffer_write(cid(1), t, n, 0, b"hello");
        assert_eq!(r, BufferWriteResult::Accepted);
        let (bytes, full) = buf.read(cid(1), 0, 5);
        assert_eq!(bytes, b"hello");
        assert!(full);
    }

    #[test]
    fn nospc_rejects_when_growth_exceeds_cap() {
        let buf = DsWriteBuffers::with_cap(8);
        let (t, n) = tn();
        let _ = buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAA"); // 8 bytes — fills cap
        let r = buf.buffer_write(cid(1), t, n, 8, b"X");
        assert!(matches!(
            r,
            BufferWriteResult::Nospc {
                cap: 8,
                requested: 1,
                ..
            }
        ));
        // Buffer state unchanged — overlap or growth past cap rejected.
        let (bytes, _) = buf.read(cid(1), 0, 16);
        assert_eq!(bytes.len(), 8);
    }

    #[test]
    fn overlap_writes_last_write_wins() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();
        buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAAAA"); // 10 As
        buf.buffer_write(cid(1), t, n, 4, b"BBBB"); // overwrites bytes 4-7
        let (bytes, full) = buf.read(cid(1), 0, 10);
        assert_eq!(bytes, b"AAAABBBBAA");
        assert!(full);
    }

    #[test]
    fn sparse_write_zero_pads_holes() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();
        buf.buffer_write(cid(1), t, n, 4, b"XYZ"); // first write at offset 4
        let (bytes, full) = buf.read(cid(1), 0, 7);
        assert_eq!(bytes, b"\0\0\0\0XYZ");
        assert!(full);
    }

    #[test]
    fn take_drains_and_releases_cap() {
        let buf = DsWriteBuffers::with_cap(16);
        let (t, n) = tn();
        buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAA"); // 8 of 16
        assert_eq!(buf.total_bytes(), 8);
        let entry = buf.take(cid(1)).expect("buffer present");
        assert_eq!(entry.data.len(), 8);
        assert_eq!(buf.total_bytes(), 0);
        // Drained — read returns nothing.
        let (bytes, _) = buf.read(cid(1), 0, 8);
        assert!(bytes.is_empty());
    }

    #[test]
    fn redirect_resolves_old_to_new() {
        let buf = DsWriteBuffers::with_cap(1024);
        buf.record_redirect(cid(1), cid(2));
        assert_eq!(buf.resolve(cid(1)), cid(2));
        // Composition with no redirect resolves to itself.
        assert_eq!(buf.resolve(cid(99)), cid(99));
    }

    #[test]
    fn clear_all_drops_buffers_and_redirects() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();
        buf.buffer_write(cid(1), t, n, 0, b"AAAA");
        buf.record_redirect(cid(1), cid(2));
        buf.clear_all();
        let (bytes, _) = buf.read(cid(1), 0, 4);
        assert!(bytes.is_empty());
        assert_eq!(buf.resolve(cid(1)), cid(1)); // redirect cleared
        assert_eq!(buf.total_bytes(), 0);
    }

    #[test]
    fn read_partial_returns_prefix_and_not_fully_covered() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();
        buf.buffer_write(cid(1), t, n, 0, b"AB"); // only 2 bytes buffered
        let (bytes, full) = buf.read(cid(1), 0, 8); // ask for 8
        assert_eq!(bytes, b"AB");
        assert!(!full); // caller falls back to gateway for the trailing 6
    }
}
