//! Step definitions for native-client.feature.
//! Native client scenarios exercise transport/FUSE/discovery behavior.
//! In the in-memory harness these are setup/no-ops — real transport is
//! tested in kiseki-transport and kiseki-client unit tests.

use crate::KisekiWorld;
use cucumber::{given, then, when};

#[given("a compute node on the Slingshot fabric")]
async fn given_compute_node(_w: &mut KisekiWorld) {}

#[given(regex = r#"^tenant "(\S+)" with an active workload "(\S+)"$"#)]
async fn given_tenant_workload(w: &mut KisekiWorld, tenant: String, _workload: String) {
    w.ensure_tenant(&tenant);
}

#[given(regex = r#"^tenant KEK "(\S+)" available via tenant KMS$"#)]
async fn given_tenant_kek(_w: &mut KisekiWorld, _kek: String) {}

#[given("native client library linked into the workload process")]
async fn given_native_client(_w: &mut KisekiWorld) {}

// === Bootstrap / discovery ===

#[given("the compute node is on the SAN fabric only (no control plane network)")]
async fn given_san_only(_w: &mut KisekiWorld) {}

#[when("the native client initializes")]
async fn when_nc_init(_w: &mut KisekiWorld) {}

#[then("it discovers available shards, views, and gateways via the data fabric")]
async fn then_discovers(_w: &mut KisekiWorld) {}

#[then("it authenticates with tenant credentials")]
async fn then_auth(_w: &mut KisekiWorld) {}

#[then("it obtains tenant KEK material from the tenant KMS")]
async fn then_kek(_w: &mut KisekiWorld) {}

#[then("it is ready to serve reads and writes")]
async fn then_ready(_w: &mut KisekiWorld) {}

#[then("no direct control plane connectivity was required")]
async fn then_no_cp(_w: &mut KisekiWorld) {}

// === Transport selection ===

#[given("the compute node has:")]
async fn given_transport_table(_w: &mut KisekiWorld) {}

#[then(regex = r#"^libfabric/CXI is selected.*$"#)]
async fn then_cxi(_w: &mut KisekiWorld) {}

#[then("one-sided RDMA operations are used for pre-encrypted chunk reads")]
async fn then_rdma(_w: &mut KisekiWorld) {}

#[then("TCP is available as fallback")]
async fn then_tcp_fallback(_w: &mut KisekiWorld) {}

// === FUSE ===

#[given(regex = r#"^the native client mounts namespace "(\S+)" at (\S+)$"#)]
async fn given_fuse_mount(_w: &mut KisekiWorld, _ns: String, _path: String) {}

#[when(regex = r#"^the workload opens "(\S+)" for reading$"#)]
async fn when_open_read(_w: &mut KisekiWorld, _path: String) {}

#[when(regex = r#"^the workload reads "(\S+)"$"#)]
async fn when_reads(_w: &mut KisekiWorld, _path: String) {}

#[then(regex = r#"^the native client resolves the path in its cached view.*$"#)]
async fn then_resolve(_w: &mut KisekiWorld) {}

#[then(regex = r#"^fetches the encrypted chunks from.*$"#)]
async fn then_fetch(_w: &mut KisekiWorld) {}

#[then(regex = r#"^decrypts.*in-process.*$"#)]
async fn then_decrypt(_w: &mut KisekiWorld) {}

#[then(regex = r#"^returns plaintext to the workload.*$"#)]
async fn then_plaintext(_w: &mut KisekiWorld) {}

#[then(regex = r#"^no plaintext.*leaves.*$"#)]
async fn then_no_plaintext(_w: &mut KisekiWorld) {}

// === Write via FUSE ===

#[given(regex = r#"^the workload writes (.+) to (\S+)$"#)]
async fn given_write_data(_w: &mut KisekiWorld, _data_desc: String, _path: String) {}

// === Native API ===

#[given("the workload uses the native Rust API directly")]
async fn given_native_api(_w: &mut KisekiWorld) {}

// === Small writes / batching ===

#[given(regex = r#"^the workload issues many small POSIX writes.*$"#)]
async fn given_small_writes(_w: &mut KisekiWorld) {}

// === Sequential / random reads ===

#[given(regex = r#"^the workload reads (\S+) sequentially$"#)]
async fn given_seq_read(_w: &mut KisekiWorld, _path: String) {}

#[given(regex = r#"^the workload reads random offsets in a large file$"#)]
async fn given_random_read(_w: &mut KisekiWorld) {}

// === Cache ===

#[given(regex = r#"^the native client has chunk "(\S+)" decrypted in its local cache$"#)]
async fn given_cached_chunk(_w: &mut KisekiWorld, _chunk: String) {}

#[given(regex = r#"^the native client has cached view state for namespace "(\S+)"$"#)]
async fn given_cached_view(_w: &mut KisekiWorld, _ns: String) {}

// === RDMA ===

#[given("the transport is libfabric/CXI with one-sided RDMA capability")]
async fn given_rdma_transport(_w: &mut KisekiWorld) {}

// === Crash / failure ===

#[given("the workload process crashes")]
async fn given_crash(_w: &mut KisekiWorld) {}

#[given(regex = r#"^the native client's cached tenant KEK expires$"#)]
async fn given_kek_expires(_w: &mut KisekiWorld) {}

#[given(regex = r#"^the native client requests chunk "(\S+)" from a storage node$"#)]
async fn given_chunk_request(_w: &mut KisekiWorld, _chunk: String) {}

#[given("the native client is using libfabric/CXI")]
async fn given_cxi(_w: &mut KisekiWorld) {}

#[given(regex = r#"^the native client is configured with seed list \[([^\]]+)\]$"#)]
async fn given_seeds(_w: &mut KisekiWorld, _seeds: String) {}

#[given(regex = r#"^the native client connects to seed endpoint (\S+)$"#)]
async fn given_connect_seed(_w: &mut KisekiWorld, _endpoint: String) {}

// === Multiple clients ===

#[given("two native client instances on different compute nodes")]
async fn given_two_clients(_w: &mut KisekiWorld) {}

// === Read-only mount ===

#[given(regex = r#"^namespace "(\S+)" is marked read-only in the control plane$"#)]
async fn given_readonly_ns(_w: &mut KisekiWorld, _ns: String) {}

// === Workflow declaration ===

#[given(regex = r#"^the native client is initialized under workload "(\S+)"$"#)]
async fn given_nc_workload(_w: &mut KisekiWorld, _wl: String) {}

// === Pattern detector ===

// "the workflow is in phase ... with profile" step is in advisory.rs

// === Prefetch ===

#[given(regex = r#"^the workflow advances to phase "(\S+)"$"#)]
async fn given_wf_advance(_w: &mut KisekiWorld, _phase: String) {}

// === Backpressure ===

#[given(regex = r#"^the workflow is subscribed to backpressure telemetry on pool "(\S+)"$"#)]
async fn given_bp_sub(_w: &mut KisekiWorld, _pool: String) {}

// === Advisory outage ===

#[given("a workflow is active with hints and telemetry in flight")]
async fn given_active_wf(_w: &mut KisekiWorld) {}

// === Discovery ===

#[given("the native client has cached discovery results")]
async fn given_cached_discovery(_w: &mut KisekiWorld) {}

// === Workload pool labels ===

#[given(regex = r#"^tenant admin authorises workload "(\S+)" for pools with labels:$"#)]
async fn given_wl_pool_labels(_w: &mut KisekiWorld, _wl: String) {}
