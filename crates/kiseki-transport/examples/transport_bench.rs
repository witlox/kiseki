#![allow(clippy::unwrap_used, clippy::expect_used)]
// Benchmark tool — relax pedantic lints for measurement code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::match_same_arms,
    clippy::format_push_string,
    unused_variables,
    unused_assignments
)]
//! Transport benchmark harness.
//!
//! Measures latency, throughput, and concurrency for each available
//! transport. Run on lab hardware with:
//!
//! ```sh
//! cargo run --release --example transport_bench -- [options]
//! ```
//!
//! Or via the wrapper script: `tests/hw/run_transport_bench.sh`

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Benchmark results for a single transport.
#[derive(Debug)]
struct BenchResult {
    transport: String,
    /// Latency: p50 / p99 / p999 in microseconds.
    latency_p50_us: u64,
    latency_p99_us: u64,
    latency_p999_us: u64,
    /// Throughput: MB/s for a streaming 1GB transfer.
    throughput_mbps: f64,
    /// Concurrent throughput: aggregate MB/s for N parallel streams.
    concurrent_mbps: f64,
    concurrent_streams: usize,
    /// Small message rate: messages/sec for 64-byte payloads.
    small_msg_rate: f64,
}

/// Percentile from sorted durations.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Start an echo server that reads length-prefixed messages and echoes them back.
async fn echo_server(addr: SocketAddr) -> tokio::task::JoinHandle<()> {
    let listener = TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut len_buf = [0u8; 4];
                loop {
                    if stream.read_exact(&mut len_buf).await.is_err() {
                        break;
                    }
                    let msg_len = u32::from_be_bytes(len_buf) as usize;
                    if msg_len == 0 {
                        break; // shutdown signal
                    }
                    let mut buf = vec![0u8; msg_len];
                    if stream.read_exact(&mut buf).await.is_err() {
                        break;
                    }
                    // Echo back: length + data.
                    let _ = stream.write_all(&len_buf).await;
                    let _ = stream.write_all(&buf).await;
                    let _ = stream.flush().await;
                }
            });
        }
    })
}

/// Measure round-trip latency for small messages.
async fn bench_latency(addr: SocketAddr, msg_size: usize, count: usize) -> Vec<Duration> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let payload = vec![0xAB_u8; msg_size];
    let mut latencies = Vec::with_capacity(count);
    let mut recv_buf = vec![0u8; msg_size];
    let mut len_buf = [0u8; 4];

    // Warmup.
    for _ in 0..100 {
        let len = msg_size as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        stream.read_exact(&mut len_buf).await.unwrap();
        stream.read_exact(&mut recv_buf).await.unwrap();
    }

    // Measured.
    for _ in 0..count {
        let start = Instant::now();
        let len = msg_size as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        stream.read_exact(&mut len_buf).await.unwrap();
        stream.read_exact(&mut recv_buf).await.unwrap();
        latencies.push(start.elapsed());
    }

    // Send shutdown.
    let _ = stream.write_all(&0_u32.to_be_bytes()).await;
    latencies
}

/// Measure streaming throughput (unidirectional).
async fn bench_throughput(addr: SocketAddr, total_bytes: usize) -> f64 {
    let listener = TcpListener::bind(addr).await.unwrap();

    // Receiver.
    let recv_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut received = 0_usize;
        while received < total_bytes {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(_) => break,
            }
        }
        received
    });

    // Give listener time to start.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Sender.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let chunk = vec![0xCD_u8; 64 * 1024]; // 64KB chunks
    let start = Instant::now();
    let mut sent = 0_usize;
    while sent < total_bytes {
        let n = stream.write(&chunk).await.unwrap();
        sent += n;
    }
    stream.flush().await.unwrap();
    drop(stream);

    let elapsed = start.elapsed();
    let _ = recv_handle.await;

    total_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
}

/// Measure concurrent streaming throughput.
async fn bench_concurrent(addr_base: SocketAddr, streams: usize, bytes_per_stream: usize) -> f64 {
    let start = Instant::now();
    let mut handles = Vec::new();

    for i in 0..streams {
        let port = addr_base.port() + 1 + i as u16;
        let addr = SocketAddr::new(addr_base.ip(), port);
        handles.push(tokio::spawn(async move {
            bench_throughput(addr, bytes_per_stream).await
        }));
    }

    let mut total_mbps = 0.0_f64;
    for h in handles {
        total_mbps += h.await.unwrap();
    }

    let elapsed = start.elapsed();
    let total_bytes = streams * bytes_per_stream;
    // Report aggregate throughput.
    total_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
}

/// Measure small message rate (messages/sec).
async fn bench_small_msg_rate(addr: SocketAddr, msg_size: usize, duration_secs: u64) -> f64 {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let payload = vec![0xEF_u8; msg_size];
    let mut recv_buf = vec![0u8; msg_size];
    let mut len_buf = [0u8; 4];

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut count = 0_u64;

    while Instant::now() < deadline {
        let len = msg_size as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        stream.read_exact(&mut len_buf).await.unwrap();
        stream.read_exact(&mut recv_buf).await.unwrap();
        count += 1;
    }

    // Shutdown.
    let _ = stream.write_all(&0_u32.to_be_bytes()).await;

    count as f64 / duration_secs as f64
}

/// Run the full benchmark suite for TCP transport.
async fn run_tcp_bench() -> BenchResult {
    let echo_addr: SocketAddr = "127.0.0.1:19100".parse().unwrap();
    let _server = echo_server(echo_addr).await;

    // Allow server to start.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 1. Latency: 10,000 x 64-byte messages.
    eprintln!("  [tcp] Latency benchmark: 10000 x 64B round-trips...");
    let mut latencies = bench_latency(echo_addr, 64, 10_000).await;
    latencies.sort();
    let p50 = percentile(&latencies, 0.50);
    let p99 = percentile(&latencies, 0.99);
    let p999 = percentile(&latencies, 0.999);

    // 2. Throughput: 256MB streaming (reduced from 1GB for CI speed).
    eprintln!("  [tcp] Throughput benchmark: 256MB streaming...");
    let throughput_addr: SocketAddr = "127.0.0.1:19200".parse().unwrap();
    let throughput = bench_throughput(throughput_addr, 256 * 1024 * 1024).await;

    // 3. Concurrent: 4 parallel streams x 64MB each.
    eprintln!("  [tcp] Concurrent benchmark: 4 x 64MB streams...");
    let concurrent_addr: SocketAddr = "127.0.0.1:19300".parse().unwrap();
    let concurrent = bench_concurrent(concurrent_addr, 4, 64 * 1024 * 1024).await;

    // 4. Small message rate: 64-byte for 3 seconds.
    // Restart echo server for this test.
    let echo_addr2: SocketAddr = "127.0.0.1:19400".parse().unwrap();
    let _server2 = echo_server(echo_addr2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    eprintln!("  [tcp] Small message rate: 64B for 3s...");
    let msg_rate = bench_small_msg_rate(echo_addr2, 64, 3).await;

    BenchResult {
        transport: "tcp-loopback".into(),
        latency_p50_us: p50.as_micros() as u64,
        latency_p99_us: p99.as_micros() as u64,
        latency_p999_us: p999.as_micros() as u64,
        throughput_mbps: throughput,
        concurrent_mbps: concurrent,
        concurrent_streams: 4,
        small_msg_rate: msg_rate,
    }
}

/// Format results as a markdown table.
fn format_results(results: &[BenchResult]) -> String {
    let mut out = String::new();
    out.push_str("# Transport Benchmark Results\n\n");
    out.push_str(&format!("**Date**: {}\n\n", chrono_lite_date()));
    out.push_str("| Transport | p50 (µs) | p99 (µs) | p999 (µs) | Throughput (MB/s) | Concurrent (MB/s) | Streams | Msg Rate (/s) |\n");
    out.push_str("|-----------|----------|----------|-----------|-------------------|--------------------|---------|---------------|\n");
    for r in results {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.1} | {:.1} | {} | {:.0} |\n",
            r.transport,
            r.latency_p50_us,
            r.latency_p99_us,
            r.latency_p999_us,
            r.throughput_mbps,
            r.concurrent_mbps,
            r.concurrent_streams,
            r.small_msg_rate,
        ));
    }
    out.push_str("\n## Assumption Validation\n\n");
    out.push_str("| Assumption | Expected | Measured | Status |\n");
    out.push_str("|------------|----------|----------|--------|\n");
    out.push_str("| CXI < 2µs (64B) | < 2 µs | _run on Slingshot_ | PENDING |\n");
    out.push_str("| NVMe < 20µs (4KB) | < 20 µs | _run on NVMe_ | PENDING |\n");
    out.push_str("| EC < 5% CPU (4+2) | < 5% | _run under load_ | PENDING |\n");
    out.push_str("| HDD > 200 MB/s seq | > 200 MB/s | _run on HDD_ | PENDING |\n");
    out
}

/// Simple date string without pulling in chrono.
fn chrono_lite_date() -> String {
    // Use a placeholder — the shell script fills the real date.
    "$(date +%Y-%m-%d)".into()
}

#[tokio::main]
async fn main() {
    eprintln!("=== Kiseki Transport Benchmark ===\n");

    let mut results = Vec::new();

    // Always run TCP benchmark.
    eprintln!("[1/4] TCP loopback benchmark");
    results.push(run_tcp_bench().await);

    // TODO: When running on hardware with RDMA/CXI, add:
    // - VerbsIb benchmark (requires IB hardware)
    // - VerbsRoce benchmark (requires RoCE hardware)
    // - CXI benchmark (requires Slingshot hardware)

    eprintln!("\n=== Results ===\n");
    let report = format_results(&results);
    print!("{report}");

    // Write to file if requested.
    if let Ok(path) = std::env::var("BENCH_OUTPUT") {
        std::fs::write(&path, &report).unwrap();
        eprintln!("\nResults written to {path}");
    }
}
