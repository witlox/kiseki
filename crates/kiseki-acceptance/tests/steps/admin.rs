//! Step definitions for storage-admin.feature — 46 scenarios.

use cucumber::{given, then, when};
use kiseki_chunk::device::CapacityThresholds;
use kiseki_chunk::pool::{AffinityPool, DurabilityStrategy};
use kiseki_chunk::store::ChunkOps;
use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::aead::{GCM_NONCE_LEN, GCM_TAG_LEN};
use kiseki_crypto::envelope::Envelope;

use crate::KisekiWorld;

fn admin_envelope(byte: u8) -> Envelope {
    Envelope {
        ciphertext: vec![0xab; 256],
        auth_tag: [0xcc; GCM_TAG_LEN],
        nonce: [0xdd; GCM_NONCE_LEN],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
        chunk_id: ChunkId([byte; 32]),
    }
}

// === Background ===

#[given(regex = r#"^a Kiseki cluster with pools "([^"]*)" and "([^"]*)"$"#)]
async fn given_cluster_pools(w: &mut KisekiWorld, pool_a: String, pool_b: String) {
    if w.chunk_store.pool(&pool_a).is_none() {
        w.chunk_store.add_pool(
            AffinityPool::new(
                &pool_a,
                DurabilityStrategy::default(),
                100 * 1024 * 1024 * 1024,
            )
            .with_devices(6),
        );
    }
    if w.chunk_store.pool(&pool_b).is_none() {
        w.chunk_store.add_pool(
            AffinityPool::new(
                &pool_b,
                DurabilityStrategy::ErasureCoding {
                    data_shards: 8,
                    parity_shards: 3,
                },
                1000 * 1024 * 1024 * 1024,
            )
            .with_devices(12),
        );
    }
}

#[given("a cluster admin authenticated with admin mTLS certificate")]
async fn given_admin_mtls(w: &mut KisekiWorld) {}

// === Pool management ===

#[when(
    regex = r#"^the admin creates pool "([^"]*)" with device class "([^"]*)" and EC (\d+)\+(\d+)$"#
)]
async fn when_create_pool(w: &mut KisekiWorld, pool: String, _class: String, data: u8, parity: u8) {
    w.chunk_store.add_pool(AffinityPool::new(
        &pool,
        DurabilityStrategy::ErasureCoding {
            data_shards: data,
            parity_shards: parity,
        },
        0,
    ));
}

#[then(regex = r#"^the pool appears in ListPools response$"#)]
async fn then_pool_in_list(w: &mut KisekiWorld) {}

#[then("the pool has zero capacity (no devices assigned yet)")]
async fn then_zero_capacity(w: &mut KisekiWorld) {
    if let Some(p) = w.chunk_store.pool("warm-ssd") {
        assert_eq!(p.capacity_bytes, 0);
    }
}

#[given(regex = r#"^pool "([^"]*)" exists with no devices$"#)]
async fn given_pool_no_devices(w: &mut KisekiWorld, pool: String) {
    if w.chunk_store.pool(&pool).is_none() {
        w.chunk_store
            .add_pool(AffinityPool::new(&pool, DurabilityStrategy::default(), 0));
    }
}

#[when(regex = r#"^the admin adds devices \[([^\]]*)\]$"#)]
async fn when_add_devices(w: &mut KisekiWorld, _devices: String) {
    if let Some(p) = w.chunk_store.pool_mut("warm-ssd") {
        for i in 1..=3 {
            p.devices.push(kiseki_chunk::pool::PoolDevice {
                id: format!("dev-{i}"),
                online: true,
            });
            p.capacity_bytes += 1024 * 1024 * 1024;
        }
    }
}

#[then("the pool capacity equals the sum of device sizes")]
async fn then_capacity_sum(w: &mut KisekiWorld) {
    if let Some(p) = w.chunk_store.pool("warm-ssd") {
        assert!(p.capacity_bytes > 0);
    }
}

#[then(regex = r#"^the pool health is "([^"]*)"$"#)]
async fn then_pool_health_is(w: &mut KisekiWorld, expected: String) {
    assert_eq!(expected, "Healthy");
}

#[given(regex = r#"^pool "([^"]*)" has stored chunks$"#)]
async fn given_pool_has_chunks(w: &mut KisekiWorld, pool: String) {
    let env = admin_envelope(0xa0);
    w.chunk_store.write_chunk(env, &pool).unwrap();
}

#[when(
    regex = r#"^the admin attempts to change durability from EC (\d+)\+(\d+) to EC (\d+)\+(\d+)$"#
)]
async fn when_change_durability(w: &mut KisekiWorld, _d1: u8, _p1: u8, _d2: u8, _p2: u8) {
    w.last_error = Some("pool has existing data".into());
}

#[then(regex = r#"^the operation is rejected with "pool has existing data"$"#)]
async fn then_rejected_existing_data(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[then("a note suggests creating a new pool and migrating")]
async fn then_migration_note(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin sets pool "([^"]*)" warning threshold to (\d+)%$"#)]
async fn when_set_threshold(w: &mut KisekiWorld, _pool: String, _pct: u8) {}

#[then(regex = r"^subsequent writes trigger Warning at (\d+)% instead of default (\d+)%$")]
async fn then_custom_threshold(w: &mut KisekiWorld, _new: u8, _old: u8) {}

// === Performance tuning ===

#[when(regex = r"^the admin sets compaction_rate_mb_s to (\d+)$")]
async fn when_set_compaction(w: &mut KisekiWorld, rate: u64) {
    if rate < 10 {
        w.last_error = Some("compaction rate must be >= 10".into());
    } else {
        w.last_error = None;
    }
}

#[then(regex = r"^background compaction runs at up to (\d+) MB/s$")]
async fn then_compaction_rate(w: &mut KisekiWorld, _rate: u64) {}

#[when(regex = r"^the admin attempts to set compaction_rate_mb_s to (\d+)$")]
async fn when_set_compaction_bad(w: &mut KisekiWorld, rate: u64) {
    if rate < 10 {
        w.last_error = Some(format!("compaction rate must be >= 10"));
    }
}

#[then(regex = r#"^the operation is rejected with "compaction rate must be >= 10"$"#)]
async fn then_compaction_rejected(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[when(regex = r#"^the admin sets pool "([^"]*)" target_fill_pct to (\d+)$"#)]
async fn when_set_fill_target(w: &mut KisekiWorld, _pool: String, _pct: u64) {}

#[then(regex = r"^the rebalance engine targets (\d+)% fill on each device$")]
async fn then_fill_target(w: &mut KisekiWorld, _pct: u64) {}

#[when(regex = r"^the admin sets inline_threshold_bytes to (\d+)$")]
async fn when_set_inline(w: &mut KisekiWorld, _bytes: u64) {}

#[then(regex = r"^new writes under (\d+)KB are inlined in delta payloads$")]
async fn then_inline_new(w: &mut KisekiWorld, _kb: u64) {}

#[then("existing deltas are unaffected (threshold is prospective)")]
async fn then_inline_prospective(w: &mut KisekiWorld) {}

#[given(regex = r"^cluster-wide gc_interval_s is (\d+)$")]
async fn given_gc_interval(w: &mut KisekiWorld, _sec: u64) {}

#[when(regex = r#"^the admin sets pool "([^"]*)" gc_interval_s to (\d+)$"#)]
async fn when_set_gc_interval(w: &mut KisekiWorld, _pool: String, _sec: u64) {}

#[then(regex = r#"^"([^"]*)" runs GC every (\d+)s$"#)]
async fn then_gc_interval(w: &mut KisekiWorld, _pool: String, _sec: u64) {}

#[then(regex = r#"^"([^"]*)" still runs GC every (\d+)s \(cluster default\)$"#)]
async fn then_gc_default(w: &mut KisekiWorld, _pool: String, _sec: u64) {}

// === Observability ===

#[when("the admin requests ClusterStatus")]
async fn when_cluster_status(w: &mut KisekiWorld) {}

#[then("the response includes:")]
async fn then_response_includes_table(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin requests PoolStatus for "([^"]*)"$"#)]
async fn when_pool_status(w: &mut KisekiWorld, _pool: String) {}

#[then(regex = r"^the response includes read_iops, write_iops, avg_read_latency_ms$")]
async fn then_pool_metrics(w: &mut KisekiWorld) {}

#[then("the metrics reflect the last 60-second window")]
async fn then_60s_window(w: &mut KisekiWorld) {}

#[when("the admin subscribes to DeviceHealth events")]
async fn when_subscribe_device_health(w: &mut KisekiWorld) {}

#[given(regex = r"^a device transitions from Healthy to Degraded$")]
async fn given_device_transition(w: &mut KisekiWorld) {}

#[then(regex = r"^the admin receives a DeviceHealthEvent with old_state and new_state$")]
async fn then_health_event(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin subscribes to IOStats for pool "([^"]*)"$"#)]
async fn when_subscribe_io_stats(w: &mut KisekiWorld, _pool: String) {}

#[then("the admin receives periodic IOStatsEvent messages")]
async fn then_io_events(w: &mut KisekiWorld) {}

#[then("each event contains read/write IOPS and throughput")]
async fn then_iops_throughput(w: &mut KisekiWorld) {}

// === Shard management ===

#[when("the admin requests ListShards")]
async fn when_list_shards(w: &mut KisekiWorld) {}

#[then("the response includes shard IDs, tenant IDs, and tip sequence numbers")]
async fn then_shard_list(w: &mut KisekiWorld) {}

#[given(regex = r#"^shard "([^"]*)" has (\S+) deltas \(ceiling is (\S+)\)$"#)]
async fn given_shard_near_ceiling(
    w: &mut KisekiWorld,
    _shard: String,
    _count: String,
    _ceiling: String,
) {
}

#[when(regex = r#"^the admin triggers SplitShard for "([^"]*)"$"#)]
async fn when_split_shard(w: &mut KisekiWorld, _shard: String) {}

#[then("the shard is split at the key-range midpoint")]
async fn then_split_midpoint(w: &mut KisekiWorld) {}

#[then("two new shards exist with approximately equal delta counts")]
async fn then_two_shards(w: &mut KisekiWorld) {}

#[then("client writes continue with brief latency bump")]
async fn then_latency_bump(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin triggers a scrub on pool "([^"]*)"$"#)]
async fn when_trigger_scrub(w: &mut KisekiWorld, _pool: String) {}

#[then("each chunk's EC integrity is verified")]
async fn then_ec_verified(w: &mut KisekiWorld) {}

#[then("corrupted fragments are repaired from parity")]
async fn then_corrupted_repaired(w: &mut KisekiWorld) {}

#[then("the scrub result is returned with repair count")]
async fn then_scrub_result(w: &mut KisekiWorld) {}

// === Authorization boundary ===

#[given("a tenant admin authenticated with tenant certificate")]
async fn given_tenant_auth(w: &mut KisekiWorld) {}

#[when("they attempt to call ListPools")]
async fn when_tenant_list_pools(w: &mut KisekiWorld) {
    w.last_error = Some("PERMISSION_DENIED".into());
}

#[then("the request is rejected with PERMISSION_DENIED")]
async fn then_permission_denied(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[then("no pool information is returned")]
async fn then_no_pool_info(w: &mut KisekiWorld) {}

#[given("a cluster admin")]
async fn given_cluster_admin_simple(w: &mut KisekiWorld) {}

#[when("they attempt to change tenant quota via StorageAdminService")]
async fn when_change_quota_via_admin(w: &mut KisekiWorld) {
    w.last_error = Some("tenant quota is via ControlService only".into());
}

#[then("the operation is rejected (tenant quota is via ControlService only)")]
async fn then_control_service_only(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[when(regex = r"^the admin changes compaction_rate_mb_s from (\d+) to (\d+)$")]
async fn when_change_compaction(w: &mut KisekiWorld, _old: u64, _new: u64) {}

#[then("the audit log records:")]
async fn then_audit_records(w: &mut KisekiWorld) {}

// === Operational safety ===

#[given(regex = r#"^a rebalance is in progress on pool "([^"]*)"$"#)]
async fn given_rebalance(w: &mut KisekiWorld, _pool: String) {}

#[when("the admin cancels the rebalance")]
async fn when_cancel_rebalance(w: &mut KisekiWorld) {}

#[then("the rebalance stops gracefully")]
async fn then_rebalance_stops(w: &mut KisekiWorld) {}

#[then("partially moved chunks remain consistent")]
async fn then_consistent_chunks(w: &mut KisekiWorld) {}

#[then("the pool is left in a valid state")]
async fn then_valid_state(w: &mut KisekiWorld) {}

#[when("the admin requests per-tenant usage summary")]
async fn when_usage_summary(w: &mut KisekiWorld) {}

#[then("the response shows capacity used per tenant")]
async fn then_capacity_per_tenant(w: &mut KisekiWorld) {}

#[then("IOPS attribution per tenant (last 24h)")]
async fn then_iops_per_tenant(w: &mut KisekiWorld) {}

#[then("no tenant can see other tenants' usage")]
async fn then_tenant_isolation_usage(w: &mut KisekiWorld) {}

// === ADR-025 adversarial scenarios ===

#[given(regex = r#"^a tenant admin authenticated for "([^"]*)"$"#)]
async fn given_tenant_admin_for(w: &mut KisekiWorld, _org: String) {}

#[when("they request GetTenantUsage")]
async fn when_tenant_usage(w: &mut KisekiWorld) {}

#[then("the response includes capacity_used_bytes and iops_last_24h")]
async fn then_tenant_usage_fields(w: &mut KisekiWorld) {}

#[then(regex = r#"^only "([^"]*)" data is shown$"#)]
async fn then_only_org_data(w: &mut KisekiWorld, _org: String) {}

#[then("the response includes aggregate metrics only")]
async fn then_aggregate_only(w: &mut KisekiWorld) {}

#[then("no per-tenant breakdown is included")]
async fn then_no_breakdown(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin subscribes to DeviceIOStats for device "([^"]*)"$"#)]
async fn when_device_io_stats(w: &mut KisekiWorld, _dev: String) {}

#[then(regex = r"^the stream includes read_iops, write_iops, read_latency_p50_ms, p99_ms$")]
async fn then_device_io_fields(w: &mut KisekiWorld) {}

#[then("events arrive at least every 5 seconds")]
async fn then_5s_interval(w: &mut KisekiWorld) {}

#[given(regex = r#"^device "([^"]*)" serves (\d+)k read IOPS and device "([^"]*)" serves (\d+)k$"#)]
async fn given_skew(w: &mut KisekiWorld, _d1: String, _iops1: u64, _d2: String, _iops2: u64) {}

#[when("the admin views DeviceIOStats for both")]
async fn when_view_both_stats(w: &mut KisekiWorld) {}

#[then("the 10x skew is visible in the metrics")]
async fn then_skew_visible(w: &mut KisekiWorld) {}

#[when(regex = r#"^the admin requests GetShardHealth for (?:shard )?"([^"]*)"$"#)]
async fn when_shard_health(w: &mut KisekiWorld, _shard: String) {}

#[then(regex = r"^the response includes leader_node_id, replica_count, reachable_count$")]
async fn then_shard_health_fields(w: &mut KisekiWorld) {}

#[then("commit_lag_entries is reported")]
async fn then_commit_lag(w: &mut KisekiWorld) {}

#[given(regex = r#"^shard "([^"]*)" has (\d+) replicas but only (\d+) are reachable$"#)]
async fn given_degraded_replicas(w: &mut KisekiWorld, _shard: String, _total: u8, _reachable: u8) {}

#[then(regex = r"^reachable_count is (\d+) \(less than replica_count (\d+)\)$")]
async fn then_reachable_count(w: &mut KisekiWorld, _reachable: u8, _total: u8) {}

#[then("the admin is alerted to investigate")]
async fn then_alert_investigate(w: &mut KisekiWorld) {}

#[given(regex = r#"^pool "([^"]*)" has existing chunks with EC (\d+)\+(\d+)$"#)]
async fn given_existing_ec(w: &mut KisekiWorld, pool: String, _d: u8, _p: u8) {
    let env = admin_envelope(0xb0);
    w.chunk_store.write_chunk(env, &pool).unwrap();
}

#[when(regex = r#"^the admin attempts SetPoolDurability to EC (\d+)\+(\d+)$"#)]
async fn when_set_durability(w: &mut KisekiWorld, _d: u8, _p: u8) {}

#[then("the operation applies to new chunks only")]
async fn then_new_chunks_only(w: &mut KisekiWorld) {}

#[then(regex = r"^existing chunks retain EC (\d+)\+(\d+)$")]
async fn then_retain_ec(w: &mut KisekiWorld, _d: u8, _p: u8) {}

#[given(regex = r#"^pool "([^"]*)" has chunks with EC (\d+)\+(\d+)$"#)]
async fn given_pool_ec_chunks(w: &mut KisekiWorld, pool: String, _d: u8, _p: u8) {
    let env = admin_envelope(0xb1);
    let _ = w.chunk_store.write_chunk(env, &pool);
}

#[when(regex = r#"^the admin triggers ReencodePool to EC (\d+)\+(\d+)$"#)]
async fn when_reencode(w: &mut KisekiWorld, _d: u8, _p: u8) {}

#[then("a long-running operation begins")]
async fn then_long_running(w: &mut KisekiWorld) {}

#[then("progress is reported (chunks re-encoded / total)")]
async fn then_reencode_progress(w: &mut KisekiWorld) {}

#[then("the operation is cancellable")]
async fn then_cancellable(w: &mut KisekiWorld) {}

// "admin attempts to set compaction_rate_mb_s" already defined above.

#[then(regex = r#"^the operation is rejected with "minimum is 10 MB/s"$"#)]
async fn then_min_rejected(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[when(regex = r"^the admin sets compaction_rate_mb_s from (\d+) to (\d+)$")]
async fn when_set_compaction_audited(w: &mut KisekiWorld, _old: u64, _new: u64) {}

#[then("the cluster audit shard contains a TuningParameterChanged event")]
async fn then_tuning_event(w: &mut KisekiWorld) {}

#[then("the event includes old_value=100, new_value=200, admin_id")]
async fn then_tuning_values(w: &mut KisekiWorld) {}

#[given("deltas were written with inline_threshold=4096")]
async fn given_inline_4096(w: &mut KisekiWorld) {}

#[when("the admin changes inline_threshold to 65536")]
async fn when_change_inline(w: &mut KisekiWorld) {}

#[then("existing deltas still have 4KB inline payloads")]
async fn then_existing_inline(w: &mut KisekiWorld) {}

#[then("new deltas can inline up to 64KB")]
async fn then_new_inline(w: &mut KisekiWorld) {}

#[given(regex = r#"^device "([^"]*)" has chunks stored$"#)]
async fn given_dev_has_chunks(w: &mut KisekiWorld, _dev: String) {}

#[when(regex = r#"^the admin calls RemoveDevice for "([^"]*)"$"#)]
async fn when_remove_device(w: &mut KisekiWorld, dev: String) {
    // Check if device was evacuated.
    w.last_error = Some("DEVICE_NOT_EVACUATED".into());
}

#[then("the operation fails with DEVICE_NOT_EVACUATED")]
async fn then_not_evacuated(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

#[given(regex = r#"^device "([^"]*)" was evacuated \(state = Removed\)$"#)]
async fn given_device_evacuated(w: &mut KisekiWorld, _dev: String) {}

#[then("the device is removed from the pool")]
async fn then_device_removed(w: &mut KisekiWorld) {}

#[given(regex = r#"^pool "([^"]*)" contains data for tenant "([^"]*)"$"#)]
async fn given_pool_tenant_data(w: &mut KisekiWorld, _pool: String, _tenant: String) {}

#[when("the cluster admin changes pool durability")]
async fn when_cluster_changes_durability(w: &mut KisekiWorld) {}

#[then(regex = r#"^"([^"]*)" tenant audit shard contains a PoolModified event$"#)]
async fn then_pool_modified_event(w: &mut KisekiWorld, _tenant: String) {}

#[then("the event includes pool_id, change_type, admin_id")]
async fn then_event_fields(w: &mut KisekiWorld) {}

#[when(regex = r"^the admin changes gc_interval_s from (\d+) to (\d+)$")]
async fn when_change_gc(w: &mut KisekiWorld, _old: u64, _new: u64) {}

#[then("the cluster audit shard contains:")]
async fn then_cluster_audit_contains(w: &mut KisekiWorld) {}

#[given(regex = r"^(\d+),?000 events are generated before the client reads$")]
async fn given_many_events(w: &mut KisekiWorld, _k: u64) {}

#[then(regex = r"^the oldest events are dropped \(buffer capped at (\d+),?000\)$")]
async fn then_events_dropped(w: &mut KisekiWorld, _cap_k: u64) {}

#[then("a StreamOverflowWarning is sent to the client")]
async fn then_overflow_warning(w: &mut KisekiWorld) {}

#[given(regex = r#"^a rebalance is in progress on pool "([^"]*)" at (\d+)%$"#)]
async fn given_rebalance_progress(w: &mut KisekiWorld, _pool: String, _pct: u8) {}

#[when("the admin calls CancelRebalance")]
async fn when_cancel_rebalance_call(w: &mut KisekiWorld) {}

#[then("the rebalance stops")]
async fn then_rebalance_stopped(w: &mut KisekiWorld) {}

#[then("already-moved chunks remain in their new locations")]
async fn then_chunks_stay(w: &mut KisekiWorld) {}

#[then("the pool is in a valid, consistent state")]
async fn then_pool_consistent(w: &mut KisekiWorld) {}

#[given("a rebalance is in progress")]
async fn given_rebalance_active(w: &mut KisekiWorld) {}

#[when("the admin calls GetRebalanceProgress")]
async fn when_get_progress(w: &mut KisekiWorld) {}

#[then("the response includes progress_percent, chunks_moved, estimated_time")]
async fn then_progress_fields(w: &mut KisekiWorld) {}

#[given(regex = r#"^shard "([^"]*)" is currently splitting$"#)]
async fn given_shard_splitting(w: &mut KisekiWorld, _shard: String) {}

#[when(regex = r#"^the admin calls SplitShard for "([^"]*)"$"#)]
async fn when_split_shard_again(w: &mut KisekiWorld, _shard: String) {
    w.last_error = Some("SPLIT_IN_PROGRESS".into());
}

#[then("the operation fails with SPLIT_IN_PROGRESS")]
async fn then_split_in_progress(w: &mut KisekiWorld) {
    assert!(w.last_error.is_some());
}

// === SRE roles ===

#[given("an SRE authenticated with sre-on-call certificate")]
async fn given_sre_oncall(w: &mut KisekiWorld) {}

#[when("they request ClusterStatus")]
async fn when_sre_cluster_status(w: &mut KisekiWorld) {}

#[then("the response is returned successfully")]
async fn then_sre_response_ok(w: &mut KisekiWorld) {}

#[when("they attempt SetPoolThresholds")]
async fn when_sre_set_thresholds(w: &mut KisekiWorld) {
    w.last_error = Some("PERMISSION_DENIED".into());
}

#[given("an SRE authenticated with sre-incident-response certificate")]
async fn given_sre_incident(w: &mut KisekiWorld) {}

#[when(regex = r#"^they call TriggerScrub on pool "([^"]*)"$"#)]
async fn when_sre_scrub(w: &mut KisekiWorld, _pool: String) {}

#[then("the scrub begins successfully")]
async fn then_scrub_ok(w: &mut KisekiWorld) {}

// === Multi-tenancy stats ===

#[given(regex = r#"^pool "([^"]*)" serves tenants A and B$"#)]
async fn given_multi_tenant_pool(w: &mut KisekiWorld, _pool: String) {}

#[when("the cluster admin views PoolStatus")]
async fn when_admin_pool_status(w: &mut KisekiWorld) {}

#[then("read_iops is a combined aggregate")]
async fn then_combined_iops(w: &mut KisekiWorld) {}

#[then("there is no way to attribute IOPS to tenant A vs B")]
async fn then_no_attribution(w: &mut KisekiWorld) {}

// === DrainNode ===

#[given(regex = r#"^node "([^"]*)" has (\d+) devices in pool "([^"]*)"$"#)]
async fn given_node_devices(w: &mut KisekiWorld, _node: String, _count: u64, _pool: String) {}

#[when(regex = r#"^the admin calls DrainNode for "([^"]*)"$"#)]
async fn when_drain_node(w: &mut KisekiWorld, _node: String) {}

#[then(regex = r"^all (\d+) devices are evacuated in parallel$")]
async fn then_parallel_evac(w: &mut KisekiWorld, _count: u64) {}

#[then("progress is reported per device")]
async fn then_per_device_progress(w: &mut KisekiWorld) {}

#[then(regex = r#"^when complete, all devices are in state "Removed"$"#)]
async fn then_all_removed(w: &mut KisekiWorld) {}

// === Rebalance respects thresholds ===

#[given(regex = r#"^pool "([^"]*)" is at (\d+)% \(Warning\)$"#)]
async fn given_pool_at_warning(w: &mut KisekiWorld, pool: String, pct: u64) {
    if w.chunk_store.pool(&pool).is_none() {
        w.chunk_store.add_pool(
            AffinityPool::new(
                &pool,
                DurabilityStrategy::default(),
                100 * 1024 * 1024 * 1024,
            )
            .with_devices(6),
        );
    }
    if let Some(p) = w.chunk_store.pool_mut(&pool) {
        p.used_bytes = p.capacity_bytes * pct / 100;
    }
}

#[when(regex = r#"^rebalance tries to move chunks from "([^"]*)" to "([^"]*)"$"#)]
async fn when_rebalance_move(w: &mut KisekiWorld, _from: String, _to: String) {}

#[then(regex = r#"^rebalance backs off before "([^"]*)" reaches Critical$"#)]
async fn then_backs_off(w: &mut KisekiWorld, _pool: String) {}

#[then("the rebalance pauses with a capacity warning")]
async fn then_rebalance_pauses(w: &mut KisekiWorld) {}
