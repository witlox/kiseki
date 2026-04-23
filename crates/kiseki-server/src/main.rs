//! Kiseki storage server — composes all Rust crates into a single binary.
//!
//! Architecture:
//! - **Main tokio runtime**: data-path contexts (Log, Chunk, Composition,
//!   View, Gateways) + `KeyManagerService` gRPC
//! - **Advisory tokio runtime**: isolated per ADR-021 §1, separate gRPC
//!   listener for `WorkflowAdvisoryService`
//! - **TCP+TLS listener**: mTLS with Cluster CA for all data-fabric
//!   connections (I-Auth1)

// Binary crate: allow expect/unwrap for startup and top-level error handling.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[allow(dead_code)] // Wired when admin CLI binary is added.
pub(crate) mod admin;
pub(crate) mod backup;
#[allow(dead_code)] // Wired when admin subcommand is integrated into main.
pub(crate) mod cli;
mod config;
mod integrity;
pub(crate) mod metrics;
#[allow(dead_code)] // Wired at startup when data-dir migration is integrated.
pub(crate) mod migration;
mod runtime;
mod system_disk;
mod telemetry;

fn main() {
    // Load config before the runtime — it's pure env parsing, no async needed.
    let cfg = config::ServerConfig::from_env();

    // Build the main tokio runtime BEFORE tracing init.
    // The OTLP exporter (tonic gRPC) requires a tokio runtime context.
    let main_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-data")
        .build()
        .expect("failed to build main tokio runtime");

    // Initialize tracing inside the runtime so the OTLP tonic channel
    // has a tokio context for lazy connection.
    let otel_provider = main_rt.block_on(async { telemetry::init_tracing() });

    tracing::info!(
        data_addr = %cfg.data_addr,
        advisory_addr = %cfg.advisory_addr,
        "kiseki-server starting"
    );

    // Build the isolated advisory runtime (ADR-021 §1).
    let advisory_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-advisory")
        .worker_threads(2)
        .build()
        .expect("failed to build advisory tokio runtime");

    // Start advisory gRPC on the isolated runtime.
    // Clone TLS files ref for the advisory thread (both runtimes use
    // the same cert — they're the same node).
    let advisory_addr = cfg.advisory_addr;
    let advisory_tls = cfg.tls.as_ref().map(|t| config::TlsFiles {
        ca_path: t.ca_path.clone(),
        cert_path: t.cert_path.clone(),
        key_path: t.key_path.clone(),
        crl_path: t.crl_path.clone(),
    });
    let advisory_handle = advisory_rt.spawn(async move {
        if let Err(e) = runtime::run_advisory(advisory_addr, advisory_tls.as_ref()).await {
            tracing::error!(error = %e, "advisory runtime error");
        }
    });

    // Run the main server on the main runtime.
    main_rt.block_on(async move {
        if let Err(e) = runtime::run_main(cfg).await {
            tracing::error!(error = %e, "server error");
            std::process::exit(1);
        }
    });

    // Clean shutdown.
    advisory_rt.block_on(async { advisory_handle.await.ok() });
    tracing::info!("kiseki-server shut down");
    telemetry::shutdown_tracing(otel_provider);
}
