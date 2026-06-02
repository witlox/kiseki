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
    /// #146 — `op_write_ds` returns NFS4ERR_NOSPC to the kernel; the
    /// kernel pNFS client recovers by issuing COMMIT (which now
    /// drains via the chain — see `drain_post_commit`) and then
    /// retries the WRITE against the freshly-drained buffer.
    Nospc {
        /// Current buffer size (bytes in `data`, not including base).
        current: u64,
        /// Configured cap (applies to `data` only — base lives in the
        /// chain composition, not in memory).
        cap: u64,
        /// How many additional bytes were requested.
        requested: u64,
    },
    /// #146 — write is at an offset BELOW the buffer's `base_bytes`
    /// (the file region already drained to a prior composition).
    /// Overwriting committed regions would require reading the prior
    /// composition, patching it in memory, and re-uploading — a
    /// "patch composition" primitive that doesn't exist yet. For
    /// sequential workloads (fio --rw=write, append-only NFS) this
    /// case never fires; flagged for follow-up if a workload starts
    /// hitting it. Mapped to NFS4ERR_NOSPC so the kernel surfaces
    /// an error rather than silently dropping the write.
    BackwardWriteUnsupported {
        /// Where the WRITE landed (file offset).
        offset: u64,
        /// Current drain boundary (file offset).
        base_bytes: u64,
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
    /// #146 — chain root: the most recent composition produced by a
    /// COMMIT-driven drain. `None` until the first cap-hit COMMIT
    /// drains the buffer; `Some(id)` thereafter. The next
    /// `gateway.write` is constructed as `id`'s chunks + chunks
    /// derived from `data`, so the resulting composition still
    /// contains every byte ever written through this fh — the
    /// #74 correctness property is preserved.
    pub base_composition_id: Option<CompositionId>,
    /// #146 — cumulative bytes in `base_composition_id`. Zero when
    /// no drain has happened yet. Used as the file-offset origin of
    /// `data[0]`: a buffered byte at `data[i]` represents file
    /// offset `base_bytes + i`. WRITEs at offsets `< base_bytes`
    /// are not yet supported (data-overwrite case — flagged for
    /// follow-up; logged as warn and rejected with NOSPC so the
    /// kernel surfaces an error rather than silently corrupting).
    pub base_bytes: u64,
    /// Accumulated plaintext, zero-padded across holes between writes.
    /// `data[i]` represents file offset `base_bytes + i`.
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
            base_composition_id: None,
            base_bytes: 0,
            data: Vec::new(),
        });

        // #146 — translate file offset → buffer-relative offset by
        // subtracting the drain boundary (`base_bytes`). For the
        // first-COMMIT case (`base_bytes == 0`) this is a no-op; for
        // post-drain WRITEs the kernel's "retry at the same file
        // offset" pattern aligns with `data[0]` on the freshly-drained
        // buffer.
        if offset < entry.base_bytes {
            return BufferWriteResult::BackwardWriteUnsupported {
                offset,
                base_bytes: entry.base_bytes,
            };
        }
        let rel_offset = offset - entry.base_bytes;
        let off = usize::try_from(rel_offset).unwrap_or(usize::MAX);
        let end = off.saturating_add(data.len());
        let new_len_u64 = u64::try_from(end).unwrap_or(u64::MAX);
        let cur_len_u64 = u64::try_from(entry.data.len()).unwrap_or(u64::MAX);
        let growth = new_len_u64.saturating_sub(cur_len_u64);

        // Cap check on the post-write size of `data` only. The base
        // composition's bytes live in the chunk store (refcounted),
        // not in memory — so the in-memory cap bounds at most
        // `cap_bytes` worth of dirty bytes per composition,
        // regardless of file size.
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

    /// #146 — drain the in-memory buffer after a successful
    /// COMMIT-driven `gateway.write` and update the chain anchor.
    /// Called by `op_commit_ds` once the gateway has confirmed the
    /// new composition is durable + the redirect is recorded. The
    /// just-flushed bytes now live in `new_base_composition_id`'s
    /// chunks; the next WRITE will append into a freshly-empty
    /// `data` Vec at file offset `new_base_bytes`, and the next
    /// COMMIT will issue a `WriteRequest` with
    /// `base_composition_id = Some(new_base_composition_id)` so the
    /// new composition again contains every byte ever written
    /// through this fh.
    ///
    /// No-op if `composition_id` has no buffer entry (already
    /// dropped on session teardown) — drain semantics are
    /// idempotent in that case.
    pub fn drain_post_commit(
        &self,
        composition_id: CompositionId,
        new_base_composition_id: CompositionId,
        new_base_bytes: u64,
    ) {
        let mut g = self
            .inner
            .lock()
            .lock_or_die("pnfs_write_buffer.drain_post_commit");
        if let Some(entry) = g.buffers.get_mut(&composition_id) {
            let drained = u64::try_from(entry.data.len()).unwrap_or(0);
            entry.data.clear();
            entry.data.shrink_to_fit();
            entry.base_composition_id = Some(new_base_composition_id);
            entry.base_bytes = new_base_bytes;
            g.total_bytes = g.total_bytes.saturating_sub(drained);
            ds_buffer_bytes_gauge().set(i64::try_from(g.total_bytes).unwrap_or(i64::MAX));
        }
    }

    /// Snapshot the accumulated bytes for `composition_id` without
    /// removing the buffer. Returns `None` if no buffer exists.
    ///
    /// Used by `op_commit_ds` (RFC 8881 §18.3): COMMIT is "the server
    /// has stable storage for the bytes you've written so far," NOT
    /// "drop your buffer." Linux NFSv4.1 + `O_DIRECT` issues COMMIT
    /// after **each** 1 MiB WRITE; if we drained the buffer per
    /// COMMIT, the next COMMIT's new composition would only contain
    /// the latest 1 MiB (zero-padded at lower offsets) and the
    /// redirect would overwrite the previous COMMIT's composition_id
    /// — silently orphaning prior data (#74).
    ///
    /// Keeping the buffer alive across COMMITs means each COMMIT's
    /// `gateway.write` flushes the **full accumulated content** for
    /// the file, the new composition supersedes the previous one in
    /// the redirect map, and reads through the original fh4 always
    /// land on a composition containing every byte written. The cap
    /// (§D5.1, default 256 MiB) still bounds memory — buffers drain
    /// on `DESTROY_SESSION` / `DESTROY_CLIENTID` via `clear_all`.
    pub fn snapshot_for_commit(&self, composition_id: CompositionId) -> Option<BufferEntry> {
        let g = self
            .inner
            .lock()
            .lock_or_die("pnfs_write_buffer.snapshot_for_commit");
        g.buffers.get(&composition_id).cloned()
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
        // #146 — `data[i]` represents file offset `base_bytes + i`.
        // Reads below the drain boundary fall through to the gateway
        // (which serves them via the chain composition); reads at or
        // above it index into the in-memory buffer.
        if offset < entry.base_bytes {
            return (Vec::new(), false);
        }
        let rel_offset = offset - entry.base_bytes;
        let off = usize::try_from(rel_offset).unwrap_or(usize::MAX);
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

pub(crate) fn ds_write_bytes_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            Opts::new(
                "kiseki_pnfs_ds_write_bytes_total",
                "Bytes accepted into / rejected from per-composition DS \
                 write buffers (ADR-038 rev 3 §D5.1). `state` ∈ \
                 {accepted, rejected_nospc, rejected_backward}."
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

    /// #74 regression: COMMIT must NOT remove the buffer. Linux pNFS +
    /// `O_DIRECT` issues COMMIT after each 1 MiB WRITE — pre-fix `take`
    /// dropped the buffer per COMMIT, so subsequent writes started
    /// fresh and the new composition lost prior bytes. With
    /// `snapshot_for_commit` the buffer stays alive and each COMMIT
    /// flushes the full accumulated content.
    #[test]
    fn snapshot_for_commit_preserves_buffer_across_commits() {
        let buf = DsWriteBuffers::with_cap(1024);
        let (t, n) = tn();

        // WRITE 1 + COMMIT 1
        buf.buffer_write(cid(1), t, n, 0, b"AAAA");
        let snap1 = buf
            .snapshot_for_commit(cid(1))
            .expect("snapshot present after first write");
        assert_eq!(snap1.data, b"AAAA");

        // Buffer survived the snapshot.
        assert_eq!(buf.total_bytes(), 4);
        let (live, _) = buf.read(cid(1), 0, 4);
        assert_eq!(live, b"AAAA");

        // WRITE 2 at offset=4 + COMMIT 2 → snapshot has BOTH writes.
        buf.buffer_write(cid(1), t, n, 4, b"BBBB");
        let snap2 = buf
            .snapshot_for_commit(cid(1))
            .expect("snapshot present after second write");
        assert_eq!(
            snap2.data, b"AAAABBBB",
            "second commit must include first commit's bytes (#74)"
        );

        // Buffer still alive.
        assert_eq!(buf.total_bytes(), 8);
    }

    #[test]
    fn snapshot_for_commit_returns_none_for_missing_buffer() {
        let buf = DsWriteBuffers::with_cap(1024);
        assert!(buf.snapshot_for_commit(cid(42)).is_none());
    }

    /// #146 — `drain_post_commit` empties the in-memory buffer AND
    /// installs a chain anchor (`base_composition_id`, `base_bytes`).
    /// The next WRITE at file offset `base_bytes` lands at `data[0]`;
    /// the next snapshot's `base_composition_id` is the just-drained
    /// composition so `op_commit_ds` can pass it as `WriteRequest::
    /// base_composition_id` to the gateway (which prepends the prior
    /// chunks). This is the F-1 wedge fix.
    #[test]
    fn drain_post_commit_empties_buffer_and_installs_chain_anchor() {
        let buf = DsWriteBuffers::with_cap(8);
        let (t, n) = tn();

        // Fill the buffer to the cap.
        buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAA");
        assert_eq!(buf.total_bytes(), 8);

        // Drain — simulates op_commit_ds finishing a successful
        // gateway.write that produced comp_v1 with size 8.
        buf.drain_post_commit(cid(1), cid(100), 8);

        // Buffer empty.
        assert_eq!(buf.total_bytes(), 0);

        // Snapshot now carries the chain anchor — `base_composition_id`
        // is Some(comp_v1), `base_bytes` is 8, `data` is empty.
        let snap = buf
            .snapshot_for_commit(cid(1))
            .expect("entry survives drain");
        assert_eq!(snap.base_composition_id, Some(cid(100)));
        assert_eq!(snap.base_bytes, 8);
        assert!(snap.data.is_empty());
    }

    /// #146 — after a drain, the next WRITE at the file offset
    /// `base_bytes` is `Accepted` and lands at `data[0]`. This is what
    /// breaks the kernel's NOSPC ↔ COMMIT-retry loop: WRITE at offset
    /// 256 MiB (after a 256 MiB drain) succeeds against a freshly-
    /// empty buffer.
    #[test]
    fn write_after_drain_accepts_at_post_drain_offset() {
        let buf = DsWriteBuffers::with_cap(8);
        let (t, n) = tn();

        buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAA"); // fills cap
        buf.drain_post_commit(cid(1), cid(100), 8);

        // WRITE at file offset 8 = base_bytes — relative offset 0,
        // 4 bytes. Should be Accepted (buffer is empty, cap allows 8).
        let r = buf.buffer_write(cid(1), t, n, 8, b"BCDE");
        assert_eq!(r, BufferWriteResult::Accepted);

        // The buffer now has 4 bytes at relative offset 0; reading
        // from file offset 8 returns them.
        let (bytes, full) = buf.read(cid(1), 8, 4);
        assert_eq!(bytes, b"BCDE");
        assert!(full);

        // Reading from file offset 0 (below the drain boundary) misses
        // the buffer — caller falls through to the gateway via redirect.
        let (bytes, full) = buf.read(cid(1), 0, 4);
        assert!(bytes.is_empty());
        assert!(!full);
    }

    /// #146 — WRITE at a file offset BELOW the drain boundary returns
    /// `BackwardWriteUnsupported`. Sequential workloads (fio, NFS
    /// append-only) never hit this case; flagged so any workload
    /// that DOES gets a clean error instead of silent data
    /// corruption.
    #[test]
    fn write_below_drain_boundary_rejects_backward() {
        let buf = DsWriteBuffers::with_cap(16);
        let (t, n) = tn();

        buf.buffer_write(cid(1), t, n, 0, b"AAAAAAAA");
        buf.drain_post_commit(cid(1), cid(100), 8);

        // WRITE at file offset 4 < base_bytes=8.
        let r = buf.buffer_write(cid(1), t, n, 4, b"BB");
        assert!(matches!(
            r,
            BufferWriteResult::BackwardWriteUnsupported {
                offset: 4,
                base_bytes: 8,
            }
        ));
    }

    /// #146 — sustained-write simulation: WRITE 8 + DRAIN, repeated
    /// three times. Buffer never exceeds the 8-byte cap; the chain
    /// anchor walks forward each cycle so the synthesized
    /// `snapshot_for_commit` always reflects the right base
    /// composition / `base_bytes` for the next gateway.write call.
    /// This is the in-buffer slice of the per-COMMIT chain test in
    /// `pnfs_ds_server`.
    #[test]
    fn sustained_write_drain_cycle_keeps_under_cap() {
        let buf = DsWriteBuffers::with_cap(8);
        let (t, n) = tn();

        for cycle in 0u64..3 {
            let off = cycle * 8;
            let r = buf.buffer_write(cid(1), t, n, off, b"AAAAAAAA");
            assert_eq!(r, BufferWriteResult::Accepted, "cycle {cycle} write");

            // Snapshot at this point — base anchor advances each cycle.
            let snap = buf.snapshot_for_commit(cid(1)).expect("entry alive");
            assert_eq!(snap.base_bytes, off);
            assert_eq!(snap.data.len(), 8);

            // Drain. Synthesize a "new comp_id" per cycle to verify
            // the anchor is the LATEST one each time.
            let new_cid = cid(100 + u128::from(cycle));
            buf.drain_post_commit(cid(1), new_cid, off + 8);
            assert_eq!(buf.total_bytes(), 0);
        }

        // After 3 cycles, the entry's anchor is the 3rd drain's comp,
        // base_bytes = 24, data empty.
        let snap = buf.snapshot_for_commit(cid(1)).expect("entry alive");
        assert_eq!(snap.base_composition_id, Some(cid(102)));
        assert_eq!(snap.base_bytes, 24);
        assert!(snap.data.is_empty());
    }

    /// #146 — `drain_post_commit` is idempotent on a missing buffer
    /// entry (e.g. cleared by `clear_all` between snapshot and drain).
    /// Important so a COMMIT-during-session-teardown race surfaces
    /// as a no-op, not a panic.
    #[test]
    fn drain_post_commit_is_noop_for_missing_entry() {
        let buf = DsWriteBuffers::with_cap(8);
        buf.drain_post_commit(cid(42), cid(100), 1024);
        assert_eq!(buf.total_bytes(), 0);
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
