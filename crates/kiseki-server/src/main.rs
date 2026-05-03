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

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub(crate) mod admin;
pub(crate) mod admin_grpc;
pub(crate) mod backup;
pub(crate) mod cli;
mod config;
mod integrity;
pub(crate) mod metrics;
#[allow(dead_code)] // Wired at startup when data-dir migration is integrated.
pub(crate) mod migration;
mod runtime;
pub(crate) mod storage_admin;
mod system_disk;
mod telemetry;
pub(crate) mod tuning;
#[allow(dead_code)] // Wired when metrics server integrates UI router.
pub(crate) mod web;

#[allow(clippy::too_many_lines)]
fn main() {
    // Heap-profile guard. When `--features dhat` AND the binary is
    // run normally, this writes `dhat-heap.json` to CWD on exit.
    // The instrumented allocator slows the data path by ~5×; only
    // build with `--features dhat` for one-off captures.
    // `DHAT_OUTPUT_FILE` overrides the output path so a profile
    // matrix can write per-run JSON files without overwriting.
    #[cfg(feature = "dhat")]
    let _dhat = match std::env::var("DHAT_OUTPUT_FILE").ok() {
        Some(path) => dhat::Profiler::builder().file_name(path).build(),
        None => dhat::Profiler::new_heap(),
    };

    // CPU-profile guard. When `--features pprof` AND
    // `KISEKI_PPROF_OUT=/path/to.svg` is set, the server runs a
    // pprof sampling profiler at 99 Hz for the lifetime of the
    // process and writes a flamegraph SVG on graceful shutdown.
    // perf-event-paranoid bypass: pprof uses `setitimer` so it
    // works even when `perf_event_open` is locked down.
    #[cfg(feature = "pprof")]
    let _pprof_guard = match std::env::var("KISEKI_PPROF_OUT").ok() {
        Some(out) => match pprof::ProfilerGuardBuilder::default()
            .frequency(99)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
        {
            Ok(g) => Some((g, out)),
            Err(e) => {
                eprintln!("pprof: failed to build profiler guard: {e}");
                None
            }
        },
        None => None,
    };

    // Check for admin subcommand before starting the server runtime.
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] != "--help" {
        if let Some(cmd) = cli::parse_admin_args(&args) {
            run_admin_command(&cmd);
            return;
        }
    }

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

    // Emit a startup span so the OTLP exporter has something to push
    // even before request-path instrumentation lands.
    //
    // We use the OpenTelemetry tracer directly (rather than
    // `tracing::info_span!` + `tracing-opentelemetry`) because the
    // bridge from tracing → OTel via `tracing-opentelemetry` 0.32 +
    // `opentelemetry` 0.31 was silently dropping spans on this build.
    // Going through the tracer directly is the supported public API
    // and Just Works.
    // Emit a startup span via the kiseki-tracing helper so the Jaeger
    // service registry sees `kiseki-server` immediately on boot.
    // Force-flush so the span lands before any e2e probe arrives.
    {
        let _s = kiseki_tracing::span("kiseki_server.startup");
        tracing::info!(
            data_addr = %cfg.data_addr,
            advisory_addr = %cfg.advisory_addr,
            "kiseki-server starting"
        );
    }
    if let Some(ref provider) = otel_provider {
        if let Err(e) = provider.force_flush() {
            tracing::warn!(error = ?e, "OTLP startup-span flush failed");
        }
    }

    // Build the isolated advisory runtime (ADR-021 §1).
    let advisory_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("kiseki-advisory")
        .worker_threads(2)
        .build()
        .expect("failed to build advisory tokio runtime");

    // Construct the shared workflow table BEFORE spawning either
    // runtime. The advisory gRPC service mutates it; the data-path
    // gateway reads it for `x-kiseki-workflow-ref` validation. Both
    // hold the same `Arc` so a `DeclareWorkflow` RPC is observable
    // to the next S3 PUT immediately (ADR-021 §3.b).
    let shared_workflow_table =
        std::sync::Arc::new(std::sync::Mutex::new(kiseki_advisory::WorkflowTable::new()));

    // Start advisory gRPC on the isolated runtime.
    // Clone TLS files ref for the advisory thread (both runtimes use
    // the same cert — they're the same node).
    let advisory_addr = cfg.advisory_addr;
    let advisory_stream_addr = cfg.advisory_stream_addr;
    let advisory_tls = cfg.tls.as_ref().map(|t| config::TlsFiles {
        ca_path: t.ca_path.clone(),
        cert_path: t.cert_path.clone(),
        key_path: t.key_path.clone(),
        crl_path: t.crl_path.clone(),
    });
    let advisory_table_for_grpc = shared_workflow_table.clone();
    let advisory_handle = advisory_rt.spawn(async move {
        if let Err(e) = runtime::run_advisory(
            advisory_addr,
            advisory_stream_addr,
            advisory_tls.as_ref(),
            advisory_table_for_grpc,
        )
        .await
        {
            tracing::error!(error = %e, "advisory runtime error");
        }
    });

    // Run the main server on the main runtime.
    let main_workflow_table = shared_workflow_table.clone();
    main_rt.block_on(async move {
        if let Err(e) = runtime::run_main(cfg, main_workflow_table).await {
            tracing::error!(error = %e, "server error");
            std::process::exit(1);
        }
    });

    // Clean shutdown.
    advisory_rt.block_on(async { advisory_handle.await.ok() });

    // Render the pprof flamegraph BEFORE telemetry shutdown so the
    // file lands even if OTLP flush hangs. The guard is dropped
    // here automatically when the function returns.
    #[cfg(feature = "pprof")]
    if let Some((guard, out_path)) = _pprof_guard {
        match guard.report().build() {
            Ok(report) => match std::fs::File::create(&out_path) {
                Ok(file) => {
                    if let Err(e) = report.flamegraph(file) {
                        eprintln!("pprof: flamegraph render failed: {e}");
                    } else {
                        eprintln!("pprof: flamegraph written to {out_path}");
                    }
                }
                Err(e) => eprintln!("pprof: cannot create {out_path}: {e}"),
            },
            Err(e) => eprintln!("pprof: report build failed: {e}"),
        }
    }

    tracing::info!("kiseki-server shut down");
    telemetry::shutdown_tracing(otel_provider);
}

/// Run an admin CLI command and exit.
fn run_admin_command(cmd: &cli::AdminCommand) {
    match cmd {
        cli::AdminCommand::Status => {
            let status = admin::cluster_status();
            println!("{}", status.to_table());
        }
        cli::AdminCommand::MaintenanceOn => println!("Maintenance mode: ON (wire to gRPC)"),
        cli::AdminCommand::MaintenanceOff => println!("Maintenance mode: OFF (wire to gRPC)"),
        _ => println!("Command recognized but not yet wired to gRPC"),
    }
}
