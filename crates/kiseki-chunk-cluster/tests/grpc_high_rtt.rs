//! Cross-node fabric throughput under simulated cross-AZ RTT.
//!
//! Reproduces (locally, in-process) the 2026-05-03 GCP transport-
//! profile fabric quorum-loss finding: 2 s avg `PutFragment` on a
//! 28 Gbps wire. Localhost loopback is too fast to expose the bug
//! organically — RTT is sub-millisecond, so HTTP/2's default 64 KiB
//! flow-control window doesn't add measurable cost. We add 1 ms of
//! one-way latency in *each* direction via a tiny in-process TCP
//! relay (50 LOC, no tc / no sudo, no privileged container).
//!
//! The relay is a textbook "two pipes with delay queues" pattern:
//!   client → relay-port → [+1 ms queue] → server-port
//!   client ← relay-port ← [+1 ms queue] ← server-port
//!
//! With 2 ms RTT and tonic's default 64 KiB H2 stream window, a
//! 64 MiB body needs ≥1024 `WINDOW_UPDATE` round-trips → ≥2 s of
//! pure bookkeeping per call. With the production fix
//! (16 MiB window) it's 4 round-trips → ≥8 ms.
//!
//! Test asserts: 8 sequential 64 MiB `PutFragment` calls finish in
//! under 10 s. Pre-fix: ≥16 s (timeout). Post-fix: ~1 s.
//!
//! Uses **only existing public APIs** —
//! `tonic::transport::Server::builder().serve_with_shutdown(addr,_)`
//! and `tonic::transport::Channel::builder(uri).connect()`. The
//! H2 window settings on both sides match what
//! `kiseki-server::runtime` ships in production (commit f362060).

use std::sync::Arc;
use std::time::{Duration, Instant};

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::FabricPeer;
use kiseki_chunk_cluster::{ClusterChunkServer, GrpcFabricPeer};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::{Channel, Server, Uri};

// HTTP/2 flow-control windows — must match the production fix in
// `kiseki-server::runtime` (data-path Server + build_fabric_channel).
// Removing either half here will cause the test to take 16+ s
// instead of ~2 s — that's the regression check.
//
// To repro the pre-fix slowdown locally: change both consts to
// 65_535 (the h2 / hyper / tonic default) and the assertion at the
// bottom will fail with elapsed ~ 16+ s.
const H2_STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const H2_CONN_WINDOW: u32 = 32 * 1024 * 1024;

fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 32,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

fn make_envelope_64mib(seed: u8) -> Envelope {
    Envelope {
        chunk_id: ChunkId([seed; 32]),
        ciphertext: vec![seed; 64 * 1024 * 1024],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

/// Forward bytes from `src` to `dst` with `one_way_delay` of pure
/// in-flight latency. Models a fast wire with non-trivial RTT:
/// bandwidth is unbounded (kernel-loopback speed), but each byte's
/// egress timestamp = ingress timestamp + delay.
///
/// Implementation: a reader task drains `src` into a (Vec<u8>,
/// `release_at`) queue, and a writer task pops entries once their
/// `release_at` is reached. The two are decoupled by a tokio mpsc
/// channel so reads don't stall on the delay timer.
async fn relay_one_way(
    src: tokio::io::ReadHalf<TcpStream>,
    dst: tokio::io::WriteHalf<TcpStream>,
    one_way_delay: Duration,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(Instant, Vec<u8>)>(1024);

    let reader = tokio::spawn(async move {
        let mut src = src;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match src.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let release_at = Instant::now() + one_way_delay;
                    if tx.send((release_at, buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let writer = tokio::spawn(async move {
        let mut dst = dst;
        while let Some((release_at, bytes)) = rx.recv().await {
            let now = Instant::now();
            if release_at > now {
                tokio::time::sleep_until(tokio::time::Instant::from_std(release_at)).await;
            }
            if dst.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = dst.shutdown().await;
    });

    let _ = reader.await;
    let _ = writer.await;
}

/// Spawn a TCP relay: bind on an ephemeral port, accept ONE
/// connection, dial `upstream`, splice the two streams with
/// `one_way_delay` injected in each direction. Returns the
/// relay's listening address.
async fn spawn_delayed_relay(
    upstream: std::net::SocketAddr,
    one_way_delay: Duration,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("relay bind");
    let relay_addr = listener.local_addr().expect("relay addr");
    tokio::spawn(async move {
        loop {
            let Ok((client_side, _)) = listener.accept().await else {
                return;
            };
            let Ok(server_side) = TcpStream::connect(upstream).await else {
                continue;
            };
            // Disable Nagle on both halves so the delay we measure
            // is the relay's, not the kernel's coalescing.
            let _ = client_side.set_nodelay(true);
            let _ = server_side.set_nodelay(true);

            let (c_r, c_w) = tokio::io::split(client_side);
            let (s_r, s_w) = tokio::io::split(server_side);
            tokio::spawn(relay_one_way(c_r, s_w, one_way_delay));
            tokio::spawn(relay_one_way(s_r, c_w, one_way_delay));
        }
    });
    relay_addr
}

#[tokio::test(flavor = "multi_thread")]
async fn fabric_64mib_put_fragment_under_simulated_cross_az_rtt() {
    let pool = "p";
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);

    // Real server on its own ephemeral port. H2 windows must
    // match the production runtime config; otherwise the bug
    // shifts to the server's flow-control side.
    let server_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("server bind");
        l.local_addr().expect("server addr")
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .add_service(
                ClusterChunkServiceServer::new(server)
                    .max_decoding_message_size(256 * 1024 * 1024)
                    .max_encoding_message_size(256 * 1024 * 1024),
            )
            .serve_with_shutdown(server_addr, async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server");
    });

    // Insert a 1 ms one-way delay in each direction = 2 ms RTT.
    // Realistic GCP cross-AZ RTT is 0.5–2 ms; this picks the high
    // end so the test signal is unambiguous.
    let relay_addr = spawn_delayed_relay(server_addr, Duration::from_millis(1)).await;

    let uri: Uri = format!("http://{relay_addr}").parse().expect("uri");
    let channel = loop {
        match Channel::builder(uri.clone())
            .initial_stream_window_size(H2_STREAM_WINDOW)
            .initial_connection_window_size(H2_CONN_WINDOW)
            .connect()
            .await
        {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let peer = Arc::new(GrpcFabricPeer::new("test-peer", channel));

    let n: usize = 8;
    let tenant = OrgId(uuid::Uuid::nil());

    // Warm up so the first-call cost (TCP handshake + h2 SETTINGS)
    // doesn't dominate the measurement.
    let warm = make_envelope_64mib(0);
    peer.put_fragment(warm.chunk_id, 0, tenant, pool.into(), warm)
        .await
        .expect("warmup");

    let mut samples = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        let env = make_envelope_64mib(u8::try_from(i % 250 + 1).unwrap_or(1));
        let call_start = Instant::now();
        peer.put_fragment(env.chunk_id, 0, tenant, pool.into(), env)
            .await
            .expect("put_fragment");
        samples.push(call_start.elapsed());
    }
    let elapsed = start.elapsed();

    samples.sort();
    #[allow(clippy::cast_precision_loss)]
    let mib = (n as f64) * 64.0;
    let gbps = (mib * 8.0) / elapsed.as_secs_f64() / 1024.0;
    eprintln!(
        "[debug] n={n} 2 ms RTT relay; elapsed={:?} ({:.2} Gbps); \
         per-call p50={:?} p99={:?}",
        elapsed,
        gbps,
        samples[n / 2],
        samples[(n * 99) / 100],
    );

    let _ = shutdown_tx.send(());
    let _ = server_handle.await;

    // Threshold: 8 × 64 MiB at 2 ms RTT.
    //
    // Pre-fix (default 64 KiB H2 window): 64 MiB / 64 KiB = 1024
    // round-trips × 2 ms = ≥2 s per call → ≥16 s total. The
    // production 5 s peer timeout would also fire and the calls
    // would error out.
    //
    // Post-fix (16 MiB H2 window): 64 MiB / 16 MiB = 4 round-trips
    // × 2 ms = ≥8 ms RTT overhead per call. Total ≈ 1 s.
    //
    // Budget: 10 s — leaves order-of-magnitude headroom for
    // jitter, while still failing decisively on the pre-fix
    // configuration.
    assert!(
        elapsed < Duration::from_secs(10),
        "{n} sequential 64 MiB fabric PutFragment calls under 2 ms \
         simulated RTT took {elapsed:?} ({gbps:.2} Gbps; per-call \
         p99={:?}). With H2 default 64 KiB window this would be \
         ~16+ s — the same regression the 2026-05-03 GCP transport-\
         profile run hit. Both `Server::builder` and \
         `Endpoint`/`Channel::builder` must set \
         `initial_stream_window_size` (16 MiB) and \
         `initial_connection_window_size` (32 MiB). See \
         kiseki-server::runtime::build_fabric_channel and the \
         data-path Server::builder.",
        samples[(n * 99) / 100],
    );
}
