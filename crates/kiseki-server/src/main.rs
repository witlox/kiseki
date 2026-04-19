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

mod config;
mod runtime;

fn main() {
    let cfg = config::ServerConfig::from_env();
    eprintln!("kiseki-server starting on {}", cfg.data_addr);
    eprintln!("  advisory addr: {}", cfg.advisory_addr);

    // Build the main tokio runtime.
    let main_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-data")
        .build()
        .expect("failed to build main tokio runtime");

    // Build the isolated advisory runtime (ADR-021 §1).
    let advisory_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-advisory")
        .worker_threads(2)
        .build()
        .expect("failed to build advisory tokio runtime");

    // Start advisory gRPC on the isolated runtime.
    let advisory_addr = cfg.advisory_addr;
    let advisory_handle = advisory_rt.spawn(async move {
        if let Err(e) = runtime::run_advisory(advisory_addr).await {
            eprintln!("advisory runtime error: {e}");
        }
    });

    // Run the main server on the main runtime.
    main_rt.block_on(async move {
        if let Err(e) = runtime::run_main(cfg).await {
            eprintln!("server error: {e}");
            std::process::exit(1);
        }
    });

    // Clean shutdown.
    advisory_rt.block_on(async { advisory_handle.await.ok() });
    eprintln!("kiseki-server shut down.");
}
