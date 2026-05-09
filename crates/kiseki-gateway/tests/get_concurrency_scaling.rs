#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]
//! GET-asymmetry probe — drives `InMemoryGateway::read` from N
//! concurrent tasks against a pre-warmed 64 MiB composition with NO
//! axum/HTTP/listener in the picture. Used to localize the ceiling
//! observed on the 2026-05-07 GCP `compact` run (S3 PUT scales to
//! ~5.4 GB/s @ 64∥ — 84 % wire — while S3 GET ceilings at ~2 GB/s
//! across 4-256∥, only 34 % wire).
//!
//! If this in-process probe shows the same flat curve, the ceiling
//! is in the gateway (top hypothesis: `DecryptCache::get` clones a
//! 64 MiB Vec under an exclusive `parking_lot::Mutex`, serializing
//! every concurrent reader through the memcpy). If reads scale
//! linearly here, the ceiling lives upstack in axum/hyper/the S3
//! listener.
//!
//! Run with:
//!     `cargo nextest run -p kiseki-gateway --run-ignored only`
//!     `--test get_concurrency_scaling -- --nocapture`
//!
//! Marked `#[ignore = "slow:"]` so Tier 1 skips it; this is an
//! investigation harness, not a regression test.

use std::sync::Arc;
use std::time::Instant;

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, WriteRequest};

const OBJ_SIZE: usize = 64 * 1024 * 1024; // 64 MiB — matches GCP perf workload
const READS_PER_TASK: usize = 8;

fn tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}
fn namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

async fn build_warm_gateway() -> (Arc<InMemoryGateway>, CompositionId) {
    let compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: namespace(),
        tenant_id: tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = Arc::new(InMemoryGateway::new(
        compositions,
        kiseki_chunk::arc_async(chunks),
        master_key,
    ));

    // PUT a 64 MiB object once — chunk encrypt + decrypt-cache
    // populate happen here. Subsequent reads hit the cache, so the
    // probe measures the cache-clone path (the hot path on the GCP
    // perf workload, which reads each 64 MiB object once after a
    // pre-warm dd loop).
    let body = vec![0xAB; OBJ_SIZE];
    let resp = gw
        .write(WriteRequest {
            tenant_id: tenant(),
            namespace_id: namespace(),
            data: body,
            name: Some("warm.bin".into()),
            conditional: None,
            workflow_ref: None,
        })
        .await
        .expect("warm-put");

    // One initial read to populate the decrypt cache so subsequent
    // reads exercise the cache-hit path.
    let _ = gw
        .read(ReadRequest {
            tenant_id: tenant(),
            namespace_id: namespace(),
            composition_id: resp.composition_id,
            offset: 0,
            length: u64::MAX,
        })
        .await
        .expect("warm-read");

    (gw, resp.composition_id)
}

async fn run_concurrency(parallelism: usize) -> f64 {
    let (gw, comp_id) = build_warm_gateway().await;

    let started = Instant::now();
    let mut handles = Vec::with_capacity(parallelism);
    for _ in 0..parallelism {
        let gw = gw.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..READS_PER_TASK {
                let resp = gw
                    .read(ReadRequest {
                        tenant_id: tenant(),
                        namespace_id: namespace(),
                        composition_id: comp_id,
                        offset: 0,
                        length: u64::MAX,
                    })
                    .await
                    .expect("read");
                assert_eq!(resp.data.len(), OBJ_SIZE);
            }
        }));
    }
    for h in handles {
        h.await.expect("task join");
    }
    let elapsed = started.elapsed().as_secs_f64();
    let total_bytes = (parallelism * READS_PER_TASK * OBJ_SIZE) as f64;
    let gbps = (total_bytes * 8.0) / elapsed / 1e9;
    eprintln!(
        "  c={:>3}  {:>5.2}s  {:>6.1} Gbps  ({:.1} GB/s, {} reads)",
        parallelism,
        elapsed,
        gbps,
        total_bytes / elapsed / 1e9,
        parallelism * READS_PER_TASK,
    );
    gbps
}

/// Drives the GET path at N=1, 4, 16, 64 against an in-process
/// `InMemoryGateway`. Prints aggregate throughput per concurrency
/// level. The shape of the curve is the diagnostic — flat means
/// gateway-level serialization; linear means the ceiling is upstack.
///
/// Asserts a soft sanity floor: at c=16, aggregate must beat the c=1
/// number. If it doesn't, the gateway is more strictly serialized
/// than concurrent (something pessimistic enough that 16 readers
/// share one reader's worth of throughput).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "slow: GET-asymmetry concurrency probe; manual run only"]
async fn gateway_read_concurrency_curve() {
    eprintln!("\nGET concurrency probe (in-process InMemoryGateway::read, 64 MiB hot object)");
    eprintln!("  workers=16, reads_per_task={READS_PER_TASK}");
    let mut results = Vec::new();
    for n in [1usize, 4, 16, 64] {
        let gbps = run_concurrency(n).await;
        results.push((n, gbps));
    }
    eprintln!("\nsummary:");
    for (n, gbps) in &results {
        eprintln!("  c={n:>3}  {gbps:>6.1} Gbps");
    }
    // Anti-serialization assertion: post-fix (Arc-wrapped cache
    // values) the read path must NOT cap aggregate throughput at
    // single-stream. Pre-fix the cache mutex held a 64 MiB Vec
    // clone, so concurrent readers serialized through one memcpy
    // and aggregate stayed flat at single-stream. The floor below
    // catches a regression of that shape; the actual ceiling at
    // c≥4 is bounded by memory bandwidth (per-task slice memcpy),
    // which on production c3-standard-44 hardware sits well above
    // the 28 Gbps wire — irrelevant to network-bound workloads.
    let g = |n: usize| {
        results
            .iter()
            .find(|(k, _)| *k == n)
            .map_or(0.0, |(_, g)| *g)
    };
    let single = g(1);
    let four = g(4);
    let sixteen = g(16);
    assert!(
        four >= single * 1.05,
        "c=4 ({four:.1} Gbps) failed to lift over c=1 ({single:.1} Gbps); \
         the read path is fully serialized again — check whether the \
         decrypt cache regressed to cloning under the mutex",
    );
    assert!(
        sixteen >= single,
        "c=16 ({sixteen:.1} Gbps) regressed below c=1 ({single:.1} Gbps); \
         pre-2026-05-09 the GET path serialized all readers through one \
         64 MiB Vec clone in DecryptCache::get — restore the Arc-wrap fix",
    );
}
