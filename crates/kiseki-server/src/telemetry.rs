//! OpenTelemetry tracing integration.
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, enables distributed
//! tracing via OTLP gRPC export (to Jaeger, Tempo, or any OTLP
//! collector).

use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber with optional OpenTelemetry export.
///
/// Returns an optional `SdkTracerProvider` that must be kept alive for the
/// duration of the process.
pub fn init_tracing() -> Option<SdkTracerProvider> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let otel_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();

    let (otel_layer, provider) = match otel_endpoint {
        Some(ref endpoint) => match build_otlp_provider(endpoint) {
            Ok(provider) => {
                let tracer = provider.tracer("kiseki");
                let layer = tracing_opentelemetry::layer().with_tracer(tracer);
                (Some(layer), Some(provider))
            }
            Err(e) => {
                eprintln!("WARNING: OTLP exporter init failed: {e}");
                (None, None)
            }
        },
        None => (None, None),
    };

    // Build subscriber: filter + fmt + optional OTel.
    // Use non-JSON format (pretty) — the OTel layer handles structured export.
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();

    if let Some(ref endpoint) = otel_endpoint {
        if provider.is_some() {
            tracing::info!(endpoint = %endpoint, "OpenTelemetry OTLP tracing enabled");
        }
    }

    // Install for the kiseki-tracing helper. Span call sites become
    // active here; before this they were no-ops.
    if let Some(p) = provider.clone() {
        kiseki_tracing::install_global_provider(p);
    }

    provider
}

/// Build an OTLP trace provider.
fn build_otlp_provider(endpoint: &str) -> Result<SdkTracerProvider, Box<dyn std::error::Error>> {
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::trace::Sampler;

    let sample_rate: f64 = std::env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "kiseki-server".into());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let sampler = if (sample_rate - 1.0).abs() < f64::EPSILON {
        Sampler::AlwaysOn
    } else if sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(sample_rate)
    };

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(service_name)
                .build(),
        )
        .build();

    Ok(provider)
}

/// Gracefully shut down the OTLP provider, flushing pending spans.
pub fn shutdown_tracing(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider {
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "OTLP provider shutdown error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_provider_constructs_without_panic() {
        let result = build_otlp_provider("http://localhost:4317");
        assert!(result.is_ok());
    }

    #[test]
    fn shutdown_none_is_noop() {
        shutdown_tracing(None);
    }
}
