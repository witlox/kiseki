#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]
//! Cold-read variant of the GET-asymmetry probe — same as
//! `get_concurrency_scaling.rs` but with the decrypt cache disabled
//! via `KISEKI_DECRYPT_CACHE_TTL_MS=0`. Every read forces a fresh
//! chunk fetch + AES-GCM decrypt (mirrors the GCP perf workload's
//! 50-unique-objects pattern, where each 64 MiB read is a cache
//! miss because the working set exceeds the 256 MiB cache cap).
//!
//! The hot probe (`get_concurrency_scaling.rs`) showed the
//! `DecryptCache` mutex was the dominant serialization point on
//! cache HITS. This probe answers the follow-up: with the cache
//! out of the picture entirely, does the cold path scale, and if
//! not, which layer caps it (chunk-fetch, decrypt, or memcpy)?
//!
//! Run with:
//!     `cargo nextest run -p kiseki-gateway --run-ignored only`
//!     `--test get_cold_concurrency_scaling -- --nocapture`

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

const OBJ_SIZE: usize = 64 * 1024 * 1024;
const READS_PER_TASK: usize = 8;

fn tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}
fn namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

async fn build_gateway_with_cache_disabled() -> (Arc<InMemoryGateway>, CompositionId) {
    // Cache TTL is read at gateway-construction time. Disable it so
    // every gateway.read() falls through to chunk-fetch + decrypt.
    // The env var is process-scoped — nextest gives each test
    // binary its own process so this doesn't leak to other tests.
    std::env::set_var("KISEKI_DECRYPT_CACHE_TTL_MS", "0");

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

    let body = vec![0xAB; OBJ_SIZE];
    let resp = gw
        .write(WriteRequest {
            tenant_id: tenant(),
            namespace_id: namespace(),
            data: body,
            name: Some("cold.bin".into()),
            conditional: None,
            workflow_ref: None,
            idempotency_key: None,
        })
        .await
        .expect("warm-put");

    (gw, resp.composition_id)
}

async fn run_concurrency(parallelism: usize) -> f64 {
    let (gw, comp_id) = build_gateway_with_cache_disabled().await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "slow: cold-read GET concurrency probe; manual run only"]
async fn gateway_cold_read_concurrency_curve() {
    eprintln!("\nGET cold-read concurrency probe (cache disabled via TTL=0, 64 MiB object)");
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

    // No assertion: investigation harness, not a regression test
    // (same rationale as `get_concurrency_scaling.rs`). The
    // printed curve is the diagnostic; CI-runner contention
    // produces noise that doesn't fit a clean threshold.
}
