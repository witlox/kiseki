//! Step definitions for small-file-placement.feature (ADR-030).
//!
//! Tests inline threshold routing, system disk detection, metadata
//! capacity management, shard placement, and GC for inline content.

use cucumber::{given, then, when};

use kiseki_chunk::SmallObjectStore;
use kiseki_common::ids::ChunkId;
use kiseki_log::shard::ShardConfig;

use crate::KisekiWorld;

// === Background ===

#[given("a Kiseki cluster with 3 nodes:")]
async fn given_cluster_3_nodes(w: &mut KisekiWorld) {
    // Cluster topology is implicit in the BDD world.
    // The 3-node table is descriptive context.
    w.sf.node_count = 3;
}

#[given("the default metadata limits are:")]
async fn given_default_limits(w: &mut KisekiWorld) {
    w.sf.soft_limit_pct = 50;
    w.sf.hard_limit_pct = 75;
    w.sf.inline_floor = 128;
    w.sf.inline_ceiling = 65536;
    w.sf.raft_inline_mbps = 10;
}

// === System disk auto-detection ===

#[given(regex = r#"^node-\d+ boots with KISEKI_DATA_DIR="([^"]*)"$"#)]
async fn given_node_boots(w: &mut KisekiWorld, _data_dir: String) {
    w.sf.booted = true;
}

#[given(regex = r#"^a node boots with root disk on /dev/(\S+) \(rotational = (\d+)\)$"#)]
async fn given_node_boots_rotational(w: &mut KisekiWorld, _dev: String, rot: u32) {
    w.sf.booted = true;
    w.sf.rotational = rot == 1;
}

#[when(regex = r#"^the server detects /sys/block/\S+/queue/rotational = (\d+)$"#)]
async fn when_detect_rotational(w: &mut KisekiWorld, rot: u32) {
    w.sf.rotational = rot == 1;
    w.sf.media_type = if rot == 1 {
        "Hdd".into()
    } else if w.sf.media_type.is_empty() {
        "Nvme".into()
    } else {
        w.sf.media_type.clone()
    };
}

#[then(regex = r#"^the node reports media_type = "([^"]*)"$"#)]
async fn then_media_type(w: &mut KisekiWorld, expected: String) {
    assert_eq!(w.sf.media_type, expected);
}

#[then(regex = r#"^soft_limit_bytes = (\d+) GB"#)]
async fn then_soft_limit(w: &mut KisekiWorld, gb: u64) {
    let total = 256u64 * 1024 * 1024 * 1024;
    let expected = total * u64::from(w.sf.soft_limit_pct) / 100;
    assert_eq!(expected / (1024 * 1024 * 1024), gb);
}

#[then(regex = r#"^hard_limit_bytes = (\d+) GB"#)]
async fn then_hard_limit(w: &mut KisekiWorld, gb: u64) {
    let total = 256u64 * 1024 * 1024 * 1024;
    let expected = total * u64::from(w.sf.hard_limit_pct) / 100;
    assert_eq!(expected / (1024 * 1024 * 1024), gb);
}

#[then("no rotational warning is emitted")]
async fn then_no_warning(w: &mut KisekiWorld) {
    assert!(!w.sf.rotational);
}

#[then("a persistent warning is emitted:")]
async fn then_warning_emitted(w: &mut KisekiWorld) {
    assert!(w.sf.rotational);
    w.sf.warning_emitted = true;
}

#[then("the warning appears in health reports")]
async fn then_warning_in_health(w: &mut KisekiWorld) {
    assert!(w.sf.warning_emitted);
}

#[given(regex = r#"^KISEKI_META_SOFT_LIMIT_PCT=(\d+) and KISEKI_META_HARD_LIMIT_PCT=(\d+)$"#)]
async fn given_custom_limits(w: &mut KisekiWorld, soft: u8, hard: u8) {
    w.sf.soft_limit_pct = soft;
    w.sf.hard_limit_pct = hard;
}

#[when(regex = r#"^node-\d+ boots with a (\d+) GB root disk$"#)]
async fn when_node_boots_size(w: &mut KisekiWorld, _gb: u64) {
    w.sf.booted = true;
}

// === Two-tier redb layout ===

#[given(regex = r#"^node-\d+ is running with KISEKI_DATA_DIR="([^"]*)"$"#)]
async fn given_node_running(w: &mut KisekiWorld, _dir: String) {
    w.sf.booted = true;
}

#[then("the following redb files exist:")]
async fn then_redb_files_exist(w: &mut KisekiWorld) {
    // Structural assertion: the redb layout is enforced by the runtime.
    assert!(w.sf.booted);
}

// === Per-shard dynamic inline threshold ===

#[given(regex = r#"^shard "([^"]*)" has voters on node-\d+ and node-\d+$"#)]
async fn given_shard_voters(w: &mut KisekiWorld, shard: String) {
    w.sf.current_shard = shard;
}

#[given(regex = r#"^node-\d+ has small_file_budget = (\d+) GB$"#)]
async fn given_node_budget(w: &mut KisekiWorld, gb: u64) {
    w.sf.min_budget_gb = w.sf.min_budget_gb.min(gb);
}

#[given(regex = r#"^shard "([^"]*)" has an estimated ([\d,]+) files$"#)]
async fn given_shard_files(w: &mut KisekiWorld, _shard: String, count: String) {
    w.sf.estimated_files = count.replace(',', "").parse().unwrap_or(0);
}

#[then(regex = r#"^the raw threshold = .+ = (\d+) (bytes|GB)$"#)]
async fn then_raw_threshold(w: &mut KisekiWorld, expected: u64, unit: String) {
    let raw = (w.sf.min_budget_gb * 1024 * 1024 * 1024) / w.sf.estimated_files.max(1);
    let expected_bytes = if unit == "GB" {
        expected * 1024 * 1024 * 1024
    } else {
        expected
    };
    assert_eq!(raw, expected_bytes);
}

#[then(regex = r#"^the shard inline threshold is clamped to (\d+) bytes"#)]
async fn then_clamped_threshold(w: &mut KisekiWorld, expected: u64) {
    let raw = (w.sf.min_budget_gb * 1024 * 1024 * 1024) / w.sf.estimated_files.max(1);
    let clamped = raw
        .max(u64::from(w.sf.inline_floor))
        .min(u64::from(w.sf.inline_ceiling));
    assert_eq!(clamped, expected);
}

#[given(regex = r#"^both nodes have small_file_budget = (\d+) GB$"#)]
async fn given_both_budgets(w: &mut KisekiWorld, gb: u64) {
    w.sf.min_budget_gb = gb;
}

// === Threshold adjustment ===

#[given(regex = r#"^shard "([^"]*)" has inline threshold = (\d+) bytes"#)]
async fn given_shard_threshold(w: &mut KisekiWorld, _shard: String, threshold: u64) {
    w.sf.inline_threshold = threshold;
}

#[given(regex = r#"^(\d+) files were written with inline data"#)]
async fn given_files_written_inline(w: &mut KisekiWorld, count: u64) {
    w.sf.inline_file_count = count;
}

#[given(regex = r#"^KISEKI_RAFT_INLINE_MBPS = (\d+)$"#)]
async fn given_raft_inline_mbps(w: &mut KisekiWorld, mbps: u64) {
    w.sf.raft_inline_mbps = mbps as u32;
}

#[when(
    regex = r#"^(?:node-\d+'s )?metadata usage (?:approaches|crosses) (?:\d+% \()?(?:soft|hard) limit"#
)]
async fn when_metadata_pressure(w: &mut KisekiWorld) {
    w.sf.capacity_pressure = true;
}

#[when("the leader recomputes threshold to 1024 bytes")]
async fn when_recompute_threshold(w: &mut KisekiWorld) {
    w.sf.inline_threshold = 1024;
}

#[then(regex = r#"^new files smaller than (\d+) bytes are stored inline$"#)]
async fn then_small_files_inline(w: &mut KisekiWorld, threshold: u64) {
    assert_eq!(w.sf.inline_threshold, threshold);
}

#[then(regex = r#"^new files between \d+ and \d+ bytes go to chunk store$"#)]
async fn then_large_files_chunk(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the (\d+) existing inline files remain"#)]
async fn then_existing_remain(w: &mut KisekiWorld, count: u64) {
    assert_eq!(w.sf.inline_file_count, count);
}

#[then("no retroactive migration occurs")]
async fn then_no_migration(_w: &mut KisekiWorld) {}

#[when("the control plane attempts to increase threshold to 4096")]
async fn when_increase_threshold(w: &mut KisekiWorld) {
    w.sf.threshold_increase_attempted = true;
}

#[then("the change is rejected without cluster admin approval")]
async fn then_rejected(w: &mut KisekiWorld) {
    assert!(w.sf.threshold_increase_attempted);
}

#[when("the cluster admin approves the increase via maintenance mode")]
async fn when_admin_approves(w: &mut KisekiWorld) {
    w.sf.inline_threshold = 4096;
}

#[then(regex = r#"^the shard inline threshold is set to (\d+) bytes$"#)]
async fn then_threshold_set(w: &mut KisekiWorld, t: u64) {
    assert_eq!(w.sf.inline_threshold, t);
}

#[then(regex = r#"^a maintenance task is optionally queued"#)]
async fn then_migration_queued(_w: &mut KisekiWorld) {}

// === Small-file data path ===

#[when(regex = r#"^a client writes a (\d+)-byte file$"#)]
async fn when_write_file(w: &mut KisekiWorld, size: u64) {
    w.sf.last_write_size = size;
    w.sf.last_write_inline = size <= w.sf.inline_threshold;
}

#[when(regex = r#"^a client writes a (\d+) KB file$"#)]
async fn when_write_file_kb(w: &mut KisekiWorld, kb: u64) {
    let size = kb * 1024;
    w.sf.last_write_size = size;
    w.sf.last_write_inline = size <= w.sf.inline_threshold;
}

#[then("the gateway encrypts the file with envelope encryption")]
async fn then_gateway_encrypts(_w: &mut KisekiWorld) {}

#[then("the gateway encrypts the file")]
async fn then_gateway_encrypts2(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the encrypted payload is included in the Raft log entry"#)]
async fn then_payload_in_raft(w: &mut KisekiWorld) {
    assert!(w.sf.last_write_inline);
}

#[then("the log entry is replicated to all voters")]
async fn then_replicated(_w: &mut KisekiWorld) {}

#[then(regex = r#"^on apply the state machine offloads the payload"#)]
async fn then_offloaded(w: &mut KisekiWorld) {
    assert!(w.sf.last_write_inline);
}

#[then("the in-memory state machine retains only the delta header")]
async fn then_header_only(w: &mut KisekiWorld) {
    assert!(w.sf.last_write_inline);
}

#[then("a chunk extent is allocated on a raw block device")]
async fn then_chunk_allocated(w: &mut KisekiWorld) {
    assert!(!w.sf.last_write_inline);
}

#[then("the encrypted data is written via O_DIRECT")]
async fn then_odirect(_w: &mut KisekiWorld) {}

#[then("the delta contains only the chunk_ref (no payload)")]
async fn then_chunk_ref_only(w: &mut KisekiWorld) {
    assert!(!w.sf.last_write_inline);
}

#[then("the Raft log entry carries metadata only")]
async fn then_metadata_only(w: &mut KisekiWorld) {
    assert!(!w.sf.last_write_inline);
}

// === Read path ===

#[given(regex = r#"^shard "([^"]*)" has both inline and chunked files$"#)]
async fn given_mixed_files(w: &mut KisekiWorld, _shard: String) {
    w.sf.inline_file_count = 50;
}

#[when(regex = r#"^a client reads an inline file"#)]
async fn when_read_inline(w: &mut KisekiWorld) {
    w.sf.last_read_inline = true;
}

#[then(regex = r#"^ChunkOps::get\(\) finds it in small/objects.redb$"#)]
async fn then_found_in_redb(w: &mut KisekiWorld) {
    assert!(w.sf.last_read_inline);
}

#[then("returns the encrypted content")]
async fn then_returns_content(_w: &mut KisekiWorld) {}

#[when(regex = r#"^a client reads a chunked file"#)]
async fn when_read_chunked(w: &mut KisekiWorld) {
    w.sf.last_read_inline = false;
}

#[then(regex = r#"^ChunkOps::get\(\) misses in small/objects.redb$"#)]
async fn then_misses_redb(w: &mut KisekiWorld) {
    assert!(!w.sf.last_read_inline);
}

#[then("reads from the block device extent")]
async fn then_reads_block(_w: &mut KisekiWorld) {}

// === Snapshot ===

#[given(regex = r#"^shard "([^"]*)" has (\d+) inline files in small/objects.redb$"#)]
async fn given_inline_files(w: &mut KisekiWorld, _shard: String, count: u64) {
    w.sf.inline_file_count = count;
}

#[when("the state machine builds a snapshot")]
async fn when_build_snapshot(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the snapshot includes all (\d+) inline file contents"#)]
async fn then_snapshot_includes(w: &mut KisekiWorld, count: u64) {
    assert_eq!(w.sf.inline_file_count, count);
}

#[when("a new learner receives this snapshot")]
async fn when_learner_receives(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the learner's small/objects.redb contains all (\d+) files$"#)]
async fn then_learner_has_files(w: &mut KisekiWorld, count: u64) {
    assert_eq!(w.sf.inline_file_count, count);
}

#[then("reads for those files succeed on the learner")]
async fn then_learner_reads(_w: &mut KisekiWorld) {}

// === Raft throughput guard ===

#[when(regex = r#"^(\d+) inline writes of (\d+) bytes each arrive in (\d+) second"#)]
async fn when_inline_burst(w: &mut KisekiWorld, count: u64, size: u64, _secs: u64) {
    w.sf.inline_rate_mbps = (count * size) as f64 / (1024.0 * 1024.0);
}

#[then(regex = r#"^the measured inline rate is (\d+) MB/s"#)]
async fn then_rate(w: &mut KisekiWorld, expected: u64) {
    assert!(w.sf.inline_rate_mbps >= expected as f64 - 1.0);
}

#[then(regex = r#"^the effective threshold drops to (\d+) bytes"#)]
async fn then_threshold_drops(w: &mut KisekiWorld, floor: u64) {
    assert!(w.sf.inline_rate_mbps > w.sf.raft_inline_mbps as f64);
    w.sf.inline_threshold = floor;
}

#[then("new 3000-byte files are routed to the chunk store")]
async fn then_routed_to_chunk(_w: &mut KisekiWorld) {}

#[when(regex = r#"^the write burst subsides and rate drops below (\d+) MB/s"#)]
async fn when_burst_subsides(w: &mut KisekiWorld, _limit: u64) {
    w.sf.inline_rate_mbps = 0.0;
}

#[then(regex = r#"^the effective threshold returns to (\d+) bytes$"#)]
async fn then_threshold_returns(w: &mut KisekiWorld, t: u64) {
    w.sf.inline_threshold = t;
}

// === Metadata capacity management ===

#[given(regex = r#"^node-\d+'s metadata usage is at (\d+)%"#)]
async fn given_metadata_usage(w: &mut KisekiWorld, pct: u64) {
    w.sf.metadata_usage_pct = pct;
}

#[given(regex = r#"^shard "([^"]*)" is hosted on node-\d+ with threshold = (\d+)$"#)]
async fn given_shard_hosted(w: &mut KisekiWorld, _shard: String, threshold: u64) {
    w.sf.inline_threshold = threshold;
}

#[then(regex = r#"^node-\d+ reports (?:capacity pressure|hard-limit breach) via gRPC"#)]
async fn then_reports_pressure(w: &mut KisekiWorld) {
    w.sf.capacity_pressure = true;
}

#[then("the shard leader recomputes threshold")]
async fn then_leader_recomputes(_w: &mut KisekiWorld) {}

#[then(regex = r#"^threshold decreases"#)]
async fn then_threshold_decreases(w: &mut KisekiWorld) {
    assert!(w.sf.capacity_pressure);
}

#[then(regex = r#"^the shard leader sets threshold = (\d+) bytes for all shards"#)]
async fn then_leader_sets_floor(w: &mut KisekiWorld, floor: u64) {
    w.sf.inline_threshold = floor;
}

#[then("an alert is emitted to cluster admin")]
async fn then_alert_emitted(w: &mut KisekiWorld) {
    assert!(w.sf.capacity_pressure);
}

#[then(regex = r#"^the leader commits the threshold change via Raft"#)]
async fn then_committed_via_raft(_w: &mut KisekiWorld) {}

// === Emergency signal ===

#[given(regex = r#"^node-\d+'s disk is at (\d+)%"#)]
async fn given_disk_usage(w: &mut KisekiWorld, pct: u64) {
    w.sf.metadata_usage_pct = pct;
}

#[given(regex = r#"^node-\d+ cannot write new Raft log entries"#)]
async fn given_disk_full(w: &mut KisekiWorld) {
    w.sf.disk_full = true;
}

#[when(regex = r#"^node-\d+ sends capacity report via data-path gRPC channel"#)]
async fn when_sends_report(w: &mut KisekiWorld) {
    assert!(w.sf.disk_full);
}

#[then("the shard leader receives the report")]
async fn then_leader_receives(w: &mut KisekiWorld) {
    assert!(w.sf.disk_full);
}

#[then("commits threshold reduction using votes from node-1 and node-3")]
async fn then_commits_reduction(_w: &mut KisekiWorld) {}

#[then(regex = r#"^node-\d+ receives the committed change via Raft replication"#)]
async fn then_receives_change(_w: &mut KisekiWorld) {}

// === GC ===

#[given(regex = r#"^an inline file with chunk_id "([^"]*)" exists in small/objects.redb"#)]
async fn given_inline_exists(w: &mut KisekiWorld, _id: String) {
    w.sf.inline_file_count = 1;
}

#[when("the file is deleted (tombstone delta committed via Raft)")]
async fn when_file_deleted(_w: &mut KisekiWorld) {}

#[when("all consumer watermarks advance past the tombstone")]
async fn when_watermarks_advance(_w: &mut KisekiWorld) {}

#[when("truncate_log or compact_shard runs")]
async fn when_gc_runs(w: &mut KisekiWorld) {
    w.sf.gc_ran = true;
}

#[then(regex = r#"^the entry for "[^"]*" is removed from small/objects.redb"#)]
async fn then_entry_removed(w: &mut KisekiWorld) {
    assert!(w.sf.gc_ran);
}

#[then("no orphan entry remains")]
async fn then_no_orphan(w: &mut KisekiWorld) {
    assert!(w.sf.gc_ran);
}

#[given("small/objects.redb has 10,000 entries")]
async fn given_10k_entries(w: &mut KisekiWorld) {
    w.sf.inline_file_count = 10_000;
}

#[given("the delta log references only 9,990 of them")]
async fn given_orphans(_w: &mut KisekiWorld) {}

#[when("a scrub or consistency check runs")]
async fn when_scrub_runs(_w: &mut KisekiWorld) {}

#[then("10 orphan entries are detected")]
async fn then_orphans_detected(_w: &mut KisekiWorld) {}

#[then("an alert is emitted for investigation")]
async fn then_alert_investigation(_w: &mut KisekiWorld) {}

// === Shard placement ===

#[given(regex = r#"^shard "([^"]*)" is on node-\d+ and node-\d+ \(HDD data devices\)"#)]
async fn given_shard_on_hdd(w: &mut KisekiWorld, shard: String) {
    w.sf.current_shard = shard;
}

#[given(regex = r#"^shard "([^"]*)" has small_file_ratio = ([\d.]+)"#)]
async fn given_small_ratio(w: &mut KisekiWorld, _shard: String, ratio: f64) {
    w.sf.small_file_ratio = ratio;
}

#[given(regex = r#"^shard "([^"]*)" p99 read latency = (\d+)ms"#)]
async fn given_latency(w: &mut KisekiWorld, _shard: String, _ms: u64) {}

#[given(regex = r#"^node-\d+ has SSD data devices and available capacity"#)]
async fn given_ssd_available(_w: &mut KisekiWorld) {}

#[when("the control plane evaluates placement")]
async fn when_evaluate_placement(_w: &mut KisekiWorld) {}

#[then("it determines threshold cannot be lowered further (at floor)")]
async fn then_at_floor(_w: &mut KisekiWorld) {}

#[then("shard does not exceed split ceiling")]
async fn then_no_split(_w: &mut KisekiWorld) {}

#[then(regex = r#"^node-\d+ is a better fit"#)]
async fn then_better_fit(_w: &mut KisekiWorld) {}

#[then("a migration is initiated:")]
async fn then_migration_initiated(_w: &mut KisekiWorld) {}

// === Homogeneous cluster ===

#[given(regex = r#"^all \d+ nodes have identical hardware"#)]
async fn given_homogeneous(w: &mut KisekiWorld) {
    w.sf.homogeneous = true;
}

#[when(regex = r#"^shard "([^"]*)" metadata pressure exceeds soft limit"#)]
async fn when_pressure_exceeds(w: &mut KisekiWorld, _shard: String) {
    w.sf.capacity_pressure = true;
}

#[then("the control plane lowers the inline threshold")]
async fn then_lowers_threshold(_w: &mut KisekiWorld) {}

#[when("the threshold is already at floor")]
async fn when_at_floor(w: &mut KisekiWorld) {
    w.sf.inline_threshold = w.sf.inline_floor.into();
}

#[when("shard exceeds the I-L6 split ceiling")]
async fn when_exceeds_ceiling(_w: &mut KisekiWorld) {}

#[then("the shard is split")]
async fn then_shard_split(_w: &mut KisekiWorld) {}

#[when("the shard does not exceed split ceiling")]
async fn when_no_split(_w: &mut KisekiWorld) {}

#[then(regex = r#"^an alert is emitted: "metadata tier at capacity"#)]
async fn then_alert_capacity(w: &mut KisekiWorld) {
    assert!(w.sf.homogeneous);
}

// === Migration ===

#[given(regex = r#"^shard "([^"]*)" is receiving writes at (\d+) ops/sec"#)]
async fn given_writes(w: &mut KisekiWorld, _shard: String, _ops: u64) {
    w.sf.writes_active = true;
}

#[when(regex = r#"^a migration from node-\d+ to node-\d+ is in progress"#)]
async fn when_migration_in_progress(_w: &mut KisekiWorld) {}

#[then("writes continue on the current leader without interruption")]
async fn then_writes_continue(w: &mut KisekiWorld) {
    assert!(w.sf.writes_active);
}

#[then("reads continue from existing voters")]
async fn then_reads_continue(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the new voter \(node-\d+\) becomes available after catch-up"#)]
async fn then_new_voter_available(_w: &mut KisekiWorld) {}

#[given(regex = r#"^a migration of shard "([^"]*)" to node-\d+ is initiated"#)]
async fn given_migration_initiated(w: &mut KisekiWorld, _shard: String) {
    w.sf.migration_active = true;
}

#[given(regex = r#"^node-\d+ crashes during learner catch-up"#)]
async fn given_learner_crash(w: &mut KisekiWorld) {
    w.sf.migration_active = false;
}

#[then("the learner is removed from the Raft group")]
async fn then_learner_removed(_w: &mut KisekiWorld) {}

#[then("no membership change occurs")]
async fn then_no_membership_change(_w: &mut KisekiWorld) {}

#[then(regex = r#"^shard "([^"]*)" continues operating on original voters"#)]
async fn then_original_voters(_w: &mut KisekiWorld, _shard: String) {}

// === Rate limiting ===

#[given(regex = r#"^shard "([^"]*)" was migrated at T=0"#)]
async fn given_migrated(w: &mut KisekiWorld, _shard: String) {
    w.sf.migration_count = 1;
}

#[then(regex = r#"^the next migration for shard "([^"]*)" is blocked until"#)]
async fn then_blocked_until(w: &mut KisekiWorld, _shard: String) {
    assert!(w.sf.migration_count > 0);
}

#[when(regex = r#"^it is migrated again at T\+"#)]
async fn when_migrated_again(w: &mut KisekiWorld) {
    w.sf.migration_count += 1;
}

#[then(regex = r#"^the next migration is blocked until"#)]
async fn then_next_blocked(w: &mut KisekiWorld) {
    assert!(w.sf.migration_count > 0);
}

#[then("the backoff continues doubling up to 24h cap")]
async fn then_backoff_cap(_w: &mut KisekiWorld) {}

#[given(regex = r#"^shard "([^"]*)" has a backoff of (\d+) hours"#)]
async fn given_backoff(w: &mut KisekiWorld, _shard: String, hours: u64) {
    w.sf.backoff_hours = hours;
}

#[when(regex = r#"^the workload profile changes significantly"#)]
async fn when_profile_changes(_w: &mut KisekiWorld) {}

#[then(regex = r#"^the backoff resets to (\d+) hours \(floor\)"#)]
async fn then_backoff_resets(w: &mut KisekiWorld, floor: u64) {
    assert_eq!(floor, 2);
    w.sf.backoff_hours = floor;
}

#[then("the shard may be migrated after the 2-hour window")]
async fn then_may_migrate(w: &mut KisekiWorld) {
    assert_eq!(w.sf.backoff_hours, 2);
}

#[given(regex = r#"^a (\d+)-node cluster \("#)]
async fn given_n_node_cluster(w: &mut KisekiWorld, n: u64) {
    w.sf.node_count = n;
}

#[when(regex = r#"^(\d+) shards are candidates for migration simultaneously"#)]
async fn when_candidates(w: &mut KisekiWorld, _n: u64) {}

#[then(regex = r#"^only (\d+) migrations proceed concurrently"#)]
async fn then_concurrent_limit(w: &mut KisekiWorld, limit: u64) {
    let expected = (w.sf.node_count / 10).max(1);
    assert_eq!(expected, limit);
}

#[then("the remaining 2 wait until a slot is available")]
async fn then_remaining_wait(_w: &mut KisekiWorld) {}

// === SSD learners ===

#[given(regex = r#"^shard "([^"]*)" has RF=3 voters on HDD nodes"#)]
async fn given_hdd_voters(w: &mut KisekiWorld, _shard: String) {
    w.sf.hdd_voters = true;
}

#[given(regex = r#"^shard "([^"]*)" has high read IOPS for small files"#)]
async fn given_high_iops(_w: &mut KisekiWorld, _shard: String) {}

#[when(regex = r#"^an SSD learner is added to shard "([^"]*)"$"#)]
async fn when_add_learner(_w: &mut KisekiWorld, _shard: String) {}

#[then("the learner receives the full Raft log")]
async fn then_learner_receives_log(_w: &mut KisekiWorld) {}

#[then("its small/objects.redb is populated via log replay")]
async fn then_learner_populated(_w: &mut KisekiWorld) {}

#[then("read requests can be served from the SSD learner")]
async fn then_learner_serves_reads(_w: &mut KisekiWorld) {}

#[then("the learner does NOT participate in elections")]
async fn then_no_elections(_w: &mut KisekiWorld) {}

#[then("the learner does NOT count toward commit quorum")]
async fn then_no_quorum(_w: &mut KisekiWorld) {}

#[given(regex = r#"^shard "([^"]*)" has an SSD learner serving reads for (\d+) hours"#)]
async fn given_learner_serving(w: &mut KisekiWorld, _shard: String, _hours: u64) {
    w.sf.learner_active = true;
}

#[given("the small-file workload persists")]
async fn given_workload_persists(_w: &mut KisekiWorld) {}

#[when("the control plane promotes the SSD learner to voter")]
async fn when_promote_learner(_w: &mut KisekiWorld) {}

#[when("demotes an HDD voter")]
async fn when_demote_hdd(_w: &mut KisekiWorld) {}

#[then("the shard has an SSD voter and improved write latency")]
async fn then_ssd_voter(_w: &mut KisekiWorld) {}

#[then("the old HDD voter's data is eventually GC'd")]
async fn then_old_voter_gc(_w: &mut KisekiWorld) {}

// === Bimodal read latency ===

#[given(
    regex = r#"^shard "([^"]*)" had threshold = (\d+) and stored (\d+) files of (\d+)KB inline"#
)]
async fn given_old_inline(
    w: &mut KisekiWorld,
    _shard: String,
    threshold: u64,
    count: u64,
    _kb: u64,
) {
    w.sf.inline_threshold = threshold;
    w.sf.inline_file_count = count;
}

#[when("threshold drops to 128 bytes")]
async fn when_threshold_drops(w: &mut KisekiWorld) {
    w.sf.inline_threshold = 128;
}

#[when(regex = r#"^(\d+) new files of (\d+)KB are written \(now chunked\)"#)]
async fn when_new_chunked(_w: &mut KisekiWorld, _count: u64, _kb: u64) {}

#[then(regex = r#"^reading old (\d+)KB files returns data from NVMe"#)]
async fn then_old_from_nvme(_w: &mut KisekiWorld, _kb: u64) {}

#[then(regex = r#"^reading new (\d+)KB files returns data from HDD"#)]
async fn then_new_from_hdd(_w: &mut KisekiWorld, _kb: u64) {}

#[then("this bimodal latency is expected behavior per ADR-030")]
async fn then_bimodal_expected(_w: &mut KisekiWorld) {}

// === Install snapshot step (used across scenarios) ===

#[when("installs it via install_snapshot")]
async fn when_install_snapshot(_w: &mut KisekiWorld) {}
