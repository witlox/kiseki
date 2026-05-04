#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Small-file placement state (ADR-030).

#[derive(Default)]
pub struct SmallFileState {
    pub node_count: u64,
    pub soft_limit_pct: u8,
    pub hard_limit_pct: u8,
    pub inline_floor: u16,
    pub inline_ceiling: u32,
    pub raft_inline_mbps: u32,
    pub booted: bool,
    pub rotational: bool,
    pub media_type: String,
    pub warning_emitted: bool,
    pub current_shard: String,
    pub min_budget_gb: u64,
    pub estimated_files: u64,
    pub inline_threshold: u64,
    pub inline_file_count: u64,
    pub capacity_pressure: bool,
    pub threshold_increase_attempted: bool,
    pub last_write_size: u64,
    pub last_write_inline: bool,
    pub last_read_inline: bool,
    pub inline_rate_mbps: f64,
    pub metadata_usage_pct: u64,
    pub disk_full: bool,
    pub gc_ran: bool,
    pub small_file_ratio: f64,
    pub homogeneous: bool,
    pub writes_active: bool,
    pub migration_active: bool,
    pub migration_count: u64,
    pub backoff_hours: u64,
    pub hdd_voters: bool,
    pub learner_active: bool,
    pub orphan_count: u64,
    pub scrub_ran: bool,
    pub p99_latency_ms: u64,
    pub ssd_available: bool,
    pub placement_evaluated: bool,
    pub split_ceiling_exceeded: bool,
    pub migration_candidates: u64,
    pub high_read_iops: bool,
    pub learner_promoted: bool,
    pub chunked_file_count: u64,
}

impl SmallFileState {
    pub fn new() -> Self {
        Self {
            node_count: 3,
            soft_limit_pct: 50,
            hard_limit_pct: 75,
            inline_floor: 128,
            inline_ceiling: 65536,
            raft_inline_mbps: 10,
            min_budget_gb: u64::MAX,
            inline_threshold: 4096,
            ..Default::default()
        }
    }
}
