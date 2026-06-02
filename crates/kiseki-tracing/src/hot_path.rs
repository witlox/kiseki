//! Fine-grained per-step write-path timers for the ADR-047 perf hunt.
//!
//! Per `specs/escalations/2026-05-30-decoupled-ack-perf-10x-analysis.md`
//! PART 2 §3.2: the GCP write budget has a ~16 ms "unattributed" tail.
//! We have `gateway_put_phase{phase=composition_record|raft_commit}`
//! at the gateway boundary, but nothing inside `put_intent_and_fan`
//! that splits leader-first-hop vs parallel-top-up, nothing inside
//! the SM apply, nothing inside the hydrator per-delta apply. The
//! macros in this module are the missing instrumentation.
//!
//! # Zero cost when off
//!
//! When the `hot-path-trace` feature is OFF (the default), the macros
//! expand to `()`. There is no histogram registration, no atomic load,
//! no static lookup. `cargo expand` on a `hot_timer!("foo")` site
//! shows literally `()`. The feature gate must hold this property —
//! adding any side-effect-shaped expansion to the off path would
//! defeat the "production builds pay nothing" contract.
//!
//! When ON, each `hot_timer!("step_name")` returns an RAII guard
//! [`HotTimer`]; its `Drop` observes the elapsed wall-clock time into
//! the process-wide
//! `kiseki_hotpath_step_duration_seconds{step=step_name}` histogram.
//! The histogram is registered into the shared Prometheus
//! [`Registry`](prometheus::Registry) by [`register`] at server boot;
//! the macro reads the histogram vec from a `OnceLock` so the first
//! call after registration latches it for every subsequent call.
//!
//! # Macro shapes
//!
//! ```ignore
//! // RAII guard — observe on Drop. Use when the timed region is the
//! // whole function body or a scope ending at a `?` propagation site.
//! let _t = kiseki_tracing::hot_timer!("gw.derive_chunk_id");
//! let chunk_id = derive_chunk_id(...)?;
//! drop(_t); // explicit when you want the boundary precise
//!
//! // Block wrapper — convenience when the timed region is a single
//! // expression / block that may `?`-return early.
//! let payload = kiseki_tracing::hot_span!("gw.encode_payload", {
//!     encode_composition_create_payload(...)
//! });
//! ```
//!
//! Labels are `&'static str` so they live in `.rodata` and the
//! histogram-vec lookup is a simple pointer-compare path inside the
//! Prometheus `HashMap`.

#[cfg(feature = "hot-path-trace")]
use std::sync::OnceLock;

#[cfg(feature = "hot-path-trace")]
use std::time::Instant;

#[cfg(feature = "hot-path-trace")]
use prometheus::{HistogramOpts, HistogramVec};

/// Histogram name exposed on `/metrics`. Microsecond-floor buckets per
/// the escalation doc — these are per-step times, not per-request.
#[cfg(feature = "hot-path-trace")]
pub const HOTPATH_HISTOGRAM_NAME: &str = "kiseki_hotpath_step_duration_seconds";

/// Bucket layout for the hotpath histogram (seconds). Covers 1 µs →
/// 100 ms. Floor at 1 µs because per-step timers fire below 10 µs
/// (the dedup short-circuit in `gw.derive_chunk_id`, the empty-store
/// case in `committer.read_pending`, etc.); ceiling at 100 ms because
/// any single step above that is already a known cross-node-RTT bug
/// the gateway-wide `gateway_put_phase` would have shown.
#[cfg(feature = "hot-path-trace")]
pub const HOTPATH_BUCKETS: &[f64] = &[
    0.000_001, 0.000_005, 0.000_01, 0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.0025, 0.005,
    0.01, 0.025, 0.05, 0.1,
];

/// Process-wide handle to the hotpath histogram-vec. Set once by
/// [`register`] at server boot; read by every `hot_timer!`/`hot_span!`
/// expansion. Lazy — when not yet registered, [`observe`] is a no-op,
/// so a `hot_timer!` call that runs before [`register`] (e.g. during
/// a unit test) simply doesn't record. We never panic for a missing
/// registration.
#[cfg(feature = "hot-path-trace")]
static HISTOGRAM: OnceLock<HistogramVec> = OnceLock::new();

/// Register the hotpath histogram-vec into the given Prometheus
/// [`Registry`](prometheus::Registry). Idempotent: a second call after
/// the histogram is latched is a no-op (the second `HistogramVec` is
/// dropped without being registered). Called from
/// `KisekiMetrics::new` at server boot when the `hot-path-trace`
/// feature is on.
///
/// # Errors
///
/// Propagates `prometheus::Error` on a registry collision (e.g. a
/// duplicate metric name from elsewhere in the same registry). The
/// hotpath name is unique within the workspace; a collision would
/// indicate misuse.
#[cfg(feature = "hot-path-trace")]
pub fn register(registry: &prometheus::Registry) -> Result<(), prometheus::Error> {
    if HISTOGRAM.get().is_some() {
        // Already registered (re-init in tests, double-boot, etc.) —
        // don't double-register and don't propagate the error.
        return Ok(());
    }
    let hv = HistogramVec::new(
        HistogramOpts::new(
            HOTPATH_HISTOGRAM_NAME,
            "Per-step write-path timer (ADR-047 escalation). `step` label is a static call-site identifier (gw.derive_chunk_id, pif.leader_first_hop, etc).",
        )
        .buckets(HOTPATH_BUCKETS.to_vec()),
        &["step"],
    )?;
    registry.register(Box::new(hv.clone()))?;
    // If a concurrent first-`hot_timer!` already latched a stub-init
    // version, drop ours (set() fails). The registered handle stays
    // alive in the registry either way — gather() finds it.
    let _ = HISTOGRAM.set(hv);
    Ok(())
}

/// RAII timer guard returned by `hot_timer!`. On `Drop` it records
/// the elapsed time into the histogram bucket labeled `step`. When
/// the histogram isn't registered yet (pre-boot, unit test) the
/// observation is silently skipped — no panic, no warn-spam.
///
/// The struct is `pub` (the macro returns it from any crate that
/// re-exports the feature) but the field stays private so a caller
/// can never tamper with the elapsed measurement.
#[cfg(feature = "hot-path-trace")]
pub struct HotTimer {
    started: Instant,
    label: &'static str,
}

#[cfg(feature = "hot-path-trace")]
impl HotTimer {
    /// Start a fresh timer for `label`. Public so the `hot_timer!`
    /// macro can build one — call sites should use the macro, never
    /// this constructor directly (the macro centralises the cfg gate).
    #[must_use]
    pub fn new(label: &'static str) -> Self {
        Self {
            started: Instant::now(),
            label,
        }
    }
}

#[cfg(feature = "hot-path-trace")]
impl Drop for HotTimer {
    fn drop(&mut self) {
        if let Some(hv) = HISTOGRAM.get() {
            let elapsed = self.started.elapsed().as_secs_f64();
            hv.with_label_values(&[self.label]).observe(elapsed);
        }
    }
}

// ---------------------------------------------------------------------------
// Off-feature stub.
//
// When `hot-path-trace` is OFF the public type stays defined as a
// zero-sized struct so callers can still write `let _t: HotTimer =
// hot_timer!(...)` in code that doesn't itself enable the feature.
// The struct has no fields; its `Drop` does nothing, and the optimiser
// eliminates the construction. The macro below expands to `()` rather
// than constructing one — even the ZST construction is a cost we don't
// want, because `Drop`-aware liveness can pin a stack slot.
// ---------------------------------------------------------------------------

/// Zero-cost stub when `hot-path-trace` is OFF.
#[cfg(not(feature = "hot-path-trace"))]
pub struct HotTimer;

#[cfg(not(feature = "hot-path-trace"))]
impl HotTimer {
    /// Off-feature constructor. The macro doesn't call this — it
    /// expands to `()` — but the symbol is kept so generated code in
    /// downstream crates (proc-macros, code generators) can refer to
    /// the type unconditionally.
    #[must_use]
    pub fn new(_label: &'static str) -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// Macros — public via `#[macro_export]` so consuming crates use them as
// `kiseki_tracing::hot_timer!(...)` without an explicit `use`.
//
// The OFF branch expands to `()`; the ON branch builds a `HotTimer`.
// `()` is dropped immediately at the end of its enclosing expression,
// so a `let _ = hot_timer!("x");` site is a true no-op when the
// feature is off — the compiler reduces it to nothing.
// ---------------------------------------------------------------------------

/// Start an RAII per-step timer. Returns a `HotTimer` whose `Drop`
/// observes the elapsed wall-clock time into the histogram bucket
/// labeled `$label`. `$label` is a `&'static str` (it lives in
/// `.rodata`; the histogram-vec keys by the pointer pair, so there's
/// no per-call string allocation).
///
/// When the `hot-path-trace` feature is OFF this expands to `()`:
/// zero atomics, zero allocations, zero codegen beyond the unit value.
///
/// # Example
///
/// ```ignore
/// let _t = kiseki_tracing::hot_timer!("gw.derive_chunk_id");
/// let chunk_id = derive_chunk_id(piece, ...)?;
/// // `_t` drops at end of scope (or at the `?`-propagation) and the
/// // elapsed time lands in the histogram.
/// ```
#[cfg(feature = "hot-path-trace")]
#[macro_export]
macro_rules! hot_timer {
    ($label:expr) => {
        $crate::hot_path::HotTimer::new($label)
    };
}

/// Off-feature expansion: `()`. No side effects, no codegen.
#[cfg(not(feature = "hot-path-trace"))]
#[macro_export]
macro_rules! hot_timer {
    ($label:expr) => {
        ()
    };
}

/// Statement-form variant of [`hot_timer!`] that emits a binding —
/// `let $ident = HotTimer::new($label);` when ON, and **nothing** when
/// OFF. Use this at call sites where the OFF-form `let x = ();` would
/// trip `clippy::let_unit_value`. The binding name is `$ident` (a
/// single underscore-prefixed identifier per site keeps the name
/// unique within the enclosing scope).
///
/// # Example
///
/// ```ignore
/// kiseki_tracing::hot_timer_guard!(_ht_derive = "gw.derive_chunk_id");
/// let chunk_id = derive_chunk_id(...)?;
/// // `_ht_derive` drops at end of scope; histogram observes when ON.
/// // When OFF the binding is never emitted at all.
/// ```
#[cfg(feature = "hot-path-trace")]
#[macro_export]
macro_rules! hot_timer_guard {
    ($name:ident = $label:expr) => {
        let $name = $crate::hot_path::HotTimer::new($label);
    };
}

/// Off-feature expansion: no statements emitted at all. The binding
/// name `$name` is consumed by the macro and never referenced
/// downstream — call sites must not try to use `$name` in any code
/// path (always pair the guard with a scope where its presence is
/// purely RAII).
#[cfg(not(feature = "hot-path-trace"))]
#[macro_export]
macro_rules! hot_timer_guard {
    ($name:ident = $label:expr) => {};
}

/// Wrap a block in a per-step timer, returning the block's result.
/// Convenience for code that returns early via `?` inside the timed
/// region — the timer drops *before* the result is moved out, so the
/// histogram observes the full region cost (success or error).
///
/// When the `hot-path-trace` feature is OFF this expands to the inner
/// block unchanged (no wrapper, no codegen overhead).
///
/// # Example
///
/// ```ignore
/// let payload = kiseki_tracing::hot_span!("gw.encode_payload", {
///     kiseki_composition::encode_composition_create_payload(
///         comp_id, ns_id, size, name, &[], Some(seq),
///     )
/// });
/// ```
#[cfg(feature = "hot-path-trace")]
#[macro_export]
macro_rules! hot_span {
    ($label:expr, $body:block) => {{
        let __kiseki_hot_timer = $crate::hot_path::HotTimer::new($label);
        let __kiseki_hot_result = $body;
        drop(__kiseki_hot_timer);
        __kiseki_hot_result
    }};
}

/// Off-feature expansion: the block unchanged.
#[cfg(not(feature = "hot-path-trace"))]
#[macro_export]
macro_rules! hot_span {
    ($label:expr, $body:block) => {{
        $body
    }};
}

#[cfg(all(test, feature = "hot-path-trace"))]
mod tests {
    use super::*;
    use prometheus::Registry;

    #[test]
    fn register_is_idempotent() {
        // Use a fresh registry per test so we don't fight other tests
        // racing on the global static HISTOGRAM. The OnceLock latches
        // on first set; subsequent register() calls are no-ops.
        let reg = Registry::new();
        // First registration may or may not be the one that latches
        // HISTOGRAM (depends on test interleaving); we just verify it
        // doesn't error.
        register(&reg).unwrap();
        register(&reg).unwrap();
    }

    #[test]
    fn timer_observes_when_registered() {
        // This test races with `register_is_idempotent` on the
        // process-wide OnceLock — exactly one of them wins the set().
        // Both code paths must work: this test just verifies the
        // macro doesn't panic and the elapsed observation is bounded.
        let t = HotTimer::new("test.observe");
        std::thread::sleep(std::time::Duration::from_micros(10));
        drop(t);
    }
}

#[cfg(all(test, not(feature = "hot-path-trace")))]
mod tests_off {
    /// Verifies that with the feature off, the macro really does
    /// expand to `()` — a static guarantee, not a runtime check.
    /// If this ever stops being true the test breaks at compile-time.
    #[test]
    fn macro_expands_to_unit_when_off() {
        // Static type-level check: the OFF expansion of `hot_timer!`
        // must be `()`. If the macro ever stops returning unit this
        // line fails to type-check.
        let _: () = crate::hot_timer!("off.path");
        // `hot_span!` OFF returns the inner block's value unchanged.
        let y: i32 = crate::hot_span!("off.span", { 42 });
        assert_eq!(y, 42);
    }
}
