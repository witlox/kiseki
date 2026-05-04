//! Minimal `OpenTelemetry` span helper.
//!
//! `tracing-opentelemetry` 0.32 + `opentelemetry` 0.31 (the current
//! ecosystem-paired versions) silently drop spans created via
//! `tracing::info_span!` — `SpanProcessor::on_end` is never called,
//! so the OTLP queue stays empty and Jaeger never sees the service.
//! This crate provides a tiny RAII wrapper that goes through the
//! `OpenTelemetry` SDK directly, bypassing the broken bridge.
//!
//! Usage:
//!
//! ```ignore
//! use kiseki_tracing::span;
//!
//! async fn create_organization(&self) -> Result<()> {
//!     let _s = span("ControlService.CreateOrganization");
//!     // ... existing handler body ...
//!     Ok(())
//! }
//! ```
//!
//! When `OpenTelemetry` isn't configured (no `KISEKI_OTEL_ENDPOINT`),
//! [`span`] returns an inert guard — zero allocations, no syscalls,
//! no locks. Call sites stay annotated unconditionally.
//!
//! The day `tracing-opentelemetry` is fixed (or replaced upstream),
//! every call site can switch to `#[tracing::instrument]` by changing
//! this crate alone.

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::OnceLock;

use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use opentelemetry_sdk::trace::SdkTracerProvider;

/// Process-wide `OpenTelemetry` provider. Set once at startup by the
/// binary's telemetry init (`kiseki_server::telemetry::init_tracing`);
/// read by every `span()` call thereafter.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Tracer name (the `OpenTelemetry` "instrumentation library"
/// identifier). All spans created by this crate share it so
/// dashboards can filter on it as a service-wide signal.
const TRACER_NAME: &str = "kiseki";

/// Install the global tracer provider. Idempotent — a second call is a
/// no-op (the second provider is silently dropped). Must be called
/// before any `span()` calls reach Jaeger; calls before installation
/// produce inert guards.
pub fn install_global_provider(provider: SdkTracerProvider) {
    let _ = PROVIDER.set(provider);
}

/// RAII guard around a started `OpenTelemetry` span.
///
/// Holds the SDK span by value so `Drop` can call `end()` and have the
/// span land in the `BatchSpanProcessor` queue for export. Holding a
/// `Span` is the contract — never expose the inner type so callers
/// can't accidentally `mem::forget` the span and skip export.
pub struct SpanGuard {
    inner: Option<opentelemetry_sdk::trace::Span>,
}

impl SpanGuard {
    /// Inert guard for the no-`OpenTelemetry` path. Drop is a no-op.
    #[must_use]
    fn inert() -> Self {
        Self { inner: None }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if let Some(mut span) = self.inner.take() {
            span.end();
        }
    }
}

/// Start a new `OpenTelemetry` span and return a guard. The span is
/// exported when the guard drops.
///
/// `name` is the span name shown in Jaeger / dashboards. Convention:
/// `Service.Method` for gRPC handlers (e.g.
/// `ControlService.CreateOrganization`), `module::function` for
/// internal spans.
///
/// If [`install_global_provider`] hasn't been called
/// (`OpenTelemetry` disabled), returns an inert guard — call sites
/// stay unconditionally annotated without paying for tracing
/// infrastructure that isn't there.
#[must_use]
pub fn span(name: &'static str) -> SpanGuard {
    let Some(provider) = PROVIDER.get() else {
        return SpanGuard::inert();
    };
    let tracer = provider.tracer(TRACER_NAME);
    SpanGuard {
        inner: Some(tracer.start(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_without_provider_is_inert() {
        // No install_global_provider() yet → inert.
        let g = span("test.no_provider");
        assert!(g.inner.is_none(), "guard must be inert when OTel disabled");
        // Drop is a no-op — must not panic.
        drop(g);
    }

    #[test]
    fn span_with_provider_holds_real_span() {
        // Provider must be a singleton — installing here would race
        // with span_without_provider_is_inert. Build a one-off
        // provider locally instead and exercise the guard's Drop.
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer(TRACER_NAME);
        let inner = tracer.start("test.local");
        let g = SpanGuard { inner: Some(inner) };
        // Drop closes the span; with no exporter configured this is
        // simply a state-machine transition that must not panic.
        drop(g);
    }
}
