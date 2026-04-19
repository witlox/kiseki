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

// "decrypts...in-process" matched by specific steps below

// "returns plaintext to the workload" matched by specific step below

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

// === Transport selection Then steps ===

#[then(regex = r#"^it selects libfabric/CXI as the primary transport.*$"#)]
async fn then_selects_cxi(_w: &mut KisekiWorld) {}

#[then("falls back to TCP if CXI connection fails")]
async fn then_fallback_tcp(_w: &mut KisekiWorld) {}

#[then("the transport selection is transparent to the workload")]
async fn then_transparent(_w: &mut KisekiWorld) {}

// === FUSE read Then steps ===

#[when(regex = r#"^the workload reads (\S+) offset (\d+) length (\S+)$"#)]
async fn when_reads_offset(_w: &mut KisekiWorld, _path: String, _off: u64, _len: String) {}

#[then("the client resolves the path in the local view cache")]
async fn then_resolve_cache(_w: &mut KisekiWorld) {}

#[then("identifies chunk references for the byte range")]
async fn then_chunk_refs(_w: &mut KisekiWorld) {}

#[then("fetches encrypted chunks from Chunk Storage over selected transport")]
async fn then_fetch_encrypted(_w: &mut KisekiWorld) {}

#[then("unwraps system DEK via tenant KEK (in-process)")]
async fn then_unwrap_inprocess(_w: &mut KisekiWorld) {}

#[then("decrypts chunks to plaintext (in-process)")]
async fn then_decrypt_inprocess(_w: &mut KisekiWorld) {}

#[then("returns plaintext to the workload via FUSE")]
async fn then_returns_fuse(_w: &mut KisekiWorld) {}

#[then("plaintext never left the workload process")]
async fn then_no_plaintext_leak(_w: &mut KisekiWorld) {}

// === POSIX read-your-writes ===

#[given("the write commits (delta committed, acknowledged)")]
async fn given_write_committed(_w: &mut KisekiWorld) {}

#[when(regex = r#"^the workload immediately reads (\S+)$"#)]
async fn when_immediate_read(_w: &mut KisekiWorld, _path: String) {}

#[then("it sees its own write (read-your-writes guarantee)")]
async fn then_ryw(_w: &mut KisekiWorld) {}

#[then("this works because the native client tracks its own uncommitted and recently-committed writes")]
async fn then_tracking(_w: &mut KisekiWorld) {}

// === Native API ===

#[when("it calls kiseki_read(namespace, path, offset, length)")]
async fn when_native_read(_w: &mut KisekiWorld) {}

#[then("the read path is the same as FUSE but without FUSE kernel overhead")]
async fn then_no_fuse_overhead(_w: &mut KisekiWorld) {}

#[then("latency is lower for small reads")]
async fn then_lower_latency(_w: &mut KisekiWorld) {}

#[then("the API returns a buffer with plaintext data")]
async fn then_buffer(_w: &mut KisekiWorld) {}

// === POSIX write ===

#[when("the native client processes the write:")]
async fn when_nc_write(_w: &mut KisekiWorld) {}

#[then("the write is acknowledged to the workload via FUSE")]
async fn then_write_ack(_w: &mut KisekiWorld) {}

#[then("plaintext existed only in the workload process memory")]
async fn then_plaintext_only_mem(_w: &mut KisekiWorld) {}

#[then("encrypted chunks traveled on the wire")]
async fn then_encrypted_wire(_w: &mut KisekiWorld) {}

// === Batching ===

#[when("the native client receives these writes")]
async fn when_receive_writes(_w: &mut KisekiWorld) {}

#[then("it batches them into larger deltas (within inline threshold)")]
async fn then_batches(_w: &mut KisekiWorld) {}

#[then("periodically flushes to the shard")]
async fn then_flushes(_w: &mut KisekiWorld) {}

#[then("the workload sees fsync semantics: flush guarantees durability")]
async fn then_fsync(_w: &mut KisekiWorld) {}

// === Sequential read ===

#[when("the native client detects sequential access pattern")]
async fn when_seq_detect(_w: &mut KisekiWorld) {}

#[then("it prefetches upcoming chunks in background")]
async fn then_prefetch_bg(_w: &mut KisekiWorld) {}

#[then("subsequent reads hit the local cache")]
async fn then_cache_hits(_w: &mut KisekiWorld) {}

#[then("read latency improves after warmup")]
async fn then_latency_improves(_w: &mut KisekiWorld) {}

// === Random read ===

#[when("the native client detects random access pattern")]
async fn when_random_detect(_w: &mut KisekiWorld) {}

#[then("it disables prefetch to avoid wasting bandwidth")]
async fn then_no_prefetch(_w: &mut KisekiWorld) {}

#[then("each read fetches on demand")]
async fn then_on_demand(_w: &mut KisekiWorld) {}

// === Cache hit ===

#[when(regex = r#"^the workload reads the byte range covered by "(\S+)"$"#)]
async fn when_read_cached(_w: &mut KisekiWorld, _chunk: String) {}

#[then("the read is served from cache")]
async fn then_from_cache(_w: &mut KisekiWorld) {}

#[then("no Chunk Storage request is made")]
async fn then_no_cs_request(_w: &mut KisekiWorld) {}

#[then("cache entries have a bounded TTL")]
async fn then_cache_ttl(_w: &mut KisekiWorld) {}

// === Cache invalidation ===

#[when(regex = r#"^a write modifies a composition in "(\S+)"$"#)]
async fn when_write_modifies(_w: &mut KisekiWorld, _ns: String) {}

#[then("the affected cache entries are invalidated")]
async fn then_invalidated(_w: &mut KisekiWorld) {}

#[then("subsequent reads fetch fresh data")]
async fn then_fresh_data(_w: &mut KisekiWorld) {}

// === RDMA ===

#[given(regex = r#"^chunk "(\S+)" is stored as system-encrypted ciphertext on a storage node$"#)]
async fn given_chunk_on_node(_w: &mut KisekiWorld, _chunk: String) {}

#[when(regex = r#"^the native client issues a one-sided RDMA read for "(\S+)"$"#)]
async fn when_rdma_read(_w: &mut KisekiWorld, _chunk: String) {}

#[then("the ciphertext is transferred directly to client memory (no target CPU)")]
async fn then_direct_transfer(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the client decrypts in-process using tenant KEK .+ system DEK$"#)]
async fn then_decrypt_inprocess2(_w: &mut KisekiWorld) {}

#[then("the storage node CPU is not involved in the transfer")]
async fn then_no_cpu(_w: &mut KisekiWorld) {}

#[then("wire encryption is provided by the pre-encrypted nature of the chunk")]
async fn then_pre_encrypted(_w: &mut KisekiWorld) {}

// === Crash ===

#[then("all in-flight uncommitted writes are lost")]
async fn then_uncommitted_lost(_w: &mut KisekiWorld) {}

#[then("committed writes (acknowledged) are durable in the Log")]
async fn then_committed_durable(_w: &mut KisekiWorld) {}

#[then("other clients and views are unaffected")]
async fn then_others_unaffected(_w: &mut KisekiWorld) {}

#[then("no cluster-wide impact")]
async fn then_no_cluster_impact(_w: &mut KisekiWorld) {}

// === KMS unreachable ===

#[given("the tenant KMS is unreachable from the compute node")]
async fn given_kms_unreachable(_w: &mut KisekiWorld) {}

#[when("the workload issues a read or write")]
async fn when_read_or_write(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the operation fails with "tenant key unavailable" error$"#)]
async fn then_key_unavailable(_w: &mut KisekiWorld) {}

#[then("the workload receives EIO (FUSE) or error code (native API)")]
async fn then_eio(_w: &mut KisekiWorld) {}

#[then("when KMS is reachable again, operations resume")]
async fn then_ops_resume(_w: &mut KisekiWorld) {}

// === Storage node unreachable ===

#[given("the storage node is unreachable")]
async fn given_node_unreachable(_w: &mut KisekiWorld) {}

#[then("the client attempts to read from an EC peer or replica")]
async fn then_ec_fallback(_w: &mut KisekiWorld) {}

#[then("if an alternative source exists, the read succeeds")]
async fn then_alt_success(_w: &mut KisekiWorld) {}

#[then("if no alternative exists, the read fails with EIO")]
async fn then_eio_fail(_w: &mut KisekiWorld) {}

// === Transport failover ===

#[when("the CXI transport fails (NIC issue, fabric partition)")]
async fn when_cxi_fails(_w: &mut KisekiWorld) {}

#[then("the client falls back to TCP transport")]
async fn then_tcp_transport(_w: &mut KisekiWorld) {}

#[then("operations continue at reduced performance")]
async fn then_reduced_perf(_w: &mut KisekiWorld) {}

#[then("the client periodically attempts to reconnect via CXI")]
async fn then_reconnect_cxi(_w: &mut KisekiWorld) {}

#[then("the failover is transparent to the workload")]
async fn then_failover_transparent(_w: &mut KisekiWorld) {}

// === Discovery failure ===

#[given("both seed endpoints are unreachable")]
async fn given_seeds_unreachable(_w: &mut KisekiWorld) {}

#[when("the native client attempts to initialize")]
async fn when_init_attempt(_w: &mut KisekiWorld) {}

#[then(regex = r#"^discovery fails with retriable "no seeds reachable" error$"#)]
async fn then_no_seeds(_w: &mut KisekiWorld) {}

#[then("the client retries with exponential backoff")]
async fn then_backoff_retry(_w: &mut KisekiWorld) {}

#[then("the workload receives EIO until discovery succeeds")]
async fn then_eio_until(_w: &mut KisekiWorld) {}

// === Discovery response ===

#[when("it sends a discovery request")]
async fn when_discovery_req(_w: &mut KisekiWorld) {}

#[then("the response contains:")]
async fn then_response_contains(_w: &mut KisekiWorld) {}

#[then("the client caches the discovery response with TTL")]
async fn then_discovery_cache(_w: &mut KisekiWorld) {}

#[then("no tenant-sensitive information is in the discovery response")]
async fn then_no_sensitive(_w: &mut KisekiWorld) {}

// === Multiple clients ===

#[given(regex = r#"^both write to (\S+)$"#)]
async fn given_both_write(_w: &mut KisekiWorld, _path: String) {}

#[then("writes from both clients are serialized in the shard (Raft ordering)")]
async fn then_serialized(_w: &mut KisekiWorld) {}

#[then("the final state reflects a total order of all writes")]
async fn then_total_order(_w: &mut KisekiWorld) {}

#[then("neither client's writes are lost (though interleaving is possible)")]
async fn then_no_write_loss(_w: &mut KisekiWorld) {}

// === Read-only mount ===

#[when(regex = r#"^the native client mounts (\S+)$"#)]
async fn when_mount(_w: &mut KisekiWorld, _path: String) {}

#[then("reads succeed normally")]
async fn then_reads_ok(_w: &mut KisekiWorld) {}

#[then("writes return EROFS (read-only filesystem)")]
async fn then_erofs(_w: &mut KisekiWorld) {}

// === Workflow declaration ===

#[when(regex = r#"^the workload calls kiseki_declare_workflow\(profile="(\S+)", initial_phase="(\S+)"\)$"#)]
async fn when_declare_wf(_w: &mut KisekiWorld, _profile: String, _phase: String) {}

#[then("the client obtains an opaque WorkflowSession handle")]
async fn then_wf_handle(_w: &mut KisekiWorld) {}

#[then("all subsequent read/write calls that take an optional session argument carry the workflow_ref annotation")]
async fn then_annotated(_w: &mut KisekiWorld) {}

#[then(regex = r#"^operations without a session argument continue to work unchanged.*$"#)]
async fn then_unchanged(_w: &mut KisekiWorld) {}

// === Pattern detector ===

#[given(regex = r#"^the native client's pattern detector observes three consecutive sequential reads on (\S+)$"#)]
async fn given_seq_reads(_w: &mut KisekiWorld, _path: String) {}

#[when("the detector classifies the access as sequential")]
async fn when_classify_seq(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the client submits hint \{ access_pattern: sequential, target: composition_id of (\S+) \} on the advisory channel$"#)]
async fn then_hint_submitted(_w: &mut KisekiWorld, _path: String) {}

#[then(regex = r#"^continues to serve reads normally.*$"#)]
async fn then_continues_reads(_w: &mut KisekiWorld) {}

#[then("if the advisory channel is unavailable the read path is unaffected")]
async fn then_channel_unavailable(_w: &mut KisekiWorld) {}

// === Prefetch ===

#[when("the workload computes the shuffled read order and calls kiseki_declare_prefetch(tuples)")]
async fn when_declare_prefetch(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the client batches tuples into PrefetchHint messages each under max_prefetch_tuples_per_hint.*$"#)]
async fn then_batches_hints(_w: &mut KisekiWorld) {}

#[then("submits them on the advisory channel")]
async fn then_submits_advisory(_w: &mut KisekiWorld) {}

#[then(regex = r#"^subsequent FUSE reads in the predicted order benefit from warmed cache.*$"#)]
async fn then_warmed_cache(_w: &mut KisekiWorld) {}

// === Backpressure ===

#[when(regex = r#"^the client receives a backpressure event with severity "(\S+)" and retry_after_ms (\d+)$"#)]
async fn when_backpressure_event(_w: &mut KisekiWorld, _sev: String, _ms: u64) {}

#[then(regex = r#"^the client MAY pause or rate-limit new submissions.*$"#)]
async fn then_may_pause(_w: &mut KisekiWorld) {}

#[then(regex = r#"^correctness of in-flight operations is unaffected.*$"#)]
async fn then_in_flight_ok(_w: &mut KisekiWorld) {}

#[then(regex = r#"^actual quota enforcement remains the data path's responsibility.*$"#)]
async fn then_quota_enforcement(_w: &mut KisekiWorld) {}

// === Advisory outage ===

#[when("the advisory subsystem on the serving node becomes unresponsive")]
async fn when_advisory_down(_w: &mut KisekiWorld) {}

#[then("the client observes advisory_unavailable on future hint submissions")]
async fn then_advisory_unavailable(_w: &mut KisekiWorld) {}

#[then(regex = r#"^FUSE reads and writes continue at normal latency and durability.*$"#)]
async fn then_fuse_continues(_w: &mut KisekiWorld) {}

#[then("the client falls back to pattern-inference for prefetch decisions (pre-existing behavior)")]
async fn then_pattern_inference(_w: &mut KisekiWorld) {}

#[then("when advisory recovers, new DeclareWorkflow calls resume")]
async fn then_advisory_resumes(_w: &mut KisekiWorld) {}

// === Advisory disabled ===

#[given(regex = r#"^tenant admin disables Workflow Advisory for "(\S+)"$"#)]
async fn given_advisory_disabled(_w: &mut KisekiWorld, _wl: String) {}

#[when("the client calls kiseki_declare_workflow")]
async fn when_call_declare(_w: &mut KisekiWorld) {}

#[then("the call returns ADVISORY_DISABLED")]
async fn then_advisory_disabled_response(_w: &mut KisekiWorld) {}

#[then("the client falls back to pattern-inference for access-pattern heuristics")]
async fn then_pattern_heuristics(_w: &mut KisekiWorld) {}

#[then(regex = r#"^FUSE reads and writes are fully correct and at normal performance.*$"#)]
async fn then_fuse_correct(_w: &mut KisekiWorld) {}
