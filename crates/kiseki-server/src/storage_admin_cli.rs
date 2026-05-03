//! `kiseki-storage` admin CLI argument parsing (ADR-025 W6).
//!
//! 26 verbs covering every `StorageAdminService` RPC. Parsing is
//! free-standing (no clap dep) so the parser stays unit-testable
//! and the dispatch binary stays slim.
//!
//! Wire surface (every verb is at the top level):
//!
//! ```text
//! kiseki-storage devices list [--pool <name>]
//! kiseki-storage devices get <device-id>
//! kiseki-storage devices add <pool> <device-id> [--capacity <bytes>] [--class <kind>]
//! kiseki-storage devices remove <device-id>
//! kiseki-storage devices evacuate <device-id> [--throughput <mb/s>]
//! kiseki-storage devices cancel-evacuation <evac-id>
//! kiseki-storage pools list
//! kiseki-storage pools get <name>
//! kiseki-storage pools create <name> --class <kind> --durability <kind> [...]
//! kiseki-storage pools set-durability <name> --durability <kind> [...]
//! kiseki-storage pools set-thresholds <name> [--warn N] [--critical N] [--readonly N] [--target N]
//! kiseki-storage pools rebalance <name> [--throughput <mb/s>]
//! kiseki-storage pools status <name>
//! kiseki-storage tuning get
//! kiseki-storage tuning set <key>=<value> [<key>=<value>...]
//! kiseki-storage cluster status
//! kiseki-storage device-health [--device <id>]
//! kiseki-storage io-stats [--pool <name>]
//! kiseki-storage shards list [--tenant <id>]
//! kiseki-storage shards get <id>
//! kiseki-storage shards split <id>
//! kiseki-storage shards merge <left> <right>
//! kiseki-storage shards maintenance <id> on|off
//! kiseki-storage scrub
//! kiseki-storage repair-chunk <chunk-id-hex>
//! kiseki-storage repairs list [--limit N]
//! ```

use std::collections::HashMap;

/// Every parsed verb. One variant per RPC. Field shapes mirror the
/// proto messages (with sensible defaults for optional fields).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // dispatch enum, not stored hot
pub enum StorageAdminCmd {
    /// `devices list [--pool <name>]`
    ListDevices { pool_name: String },
    /// `devices get <device-id>`
    GetDevice { device_id: String },
    /// `devices add <pool> <device-id> [--capacity <bytes>] [--class <kind>]`
    AddDevice {
        pool_name: String,
        device_id: String,
        capacity_bytes: u64,
        device_class: String,
    },
    /// `devices remove <device-id>`
    RemoveDevice { device_id: String },
    /// `devices evacuate <device-id> [--throughput <mb/s>]`
    EvacuateDevice {
        device_id: String,
        throughput_mb_s: u64,
    },
    /// `devices cancel-evacuation <evac-id>`
    CancelEvacuation { evacuation_id: String },
    /// `pools list`
    ListPools,
    /// `pools get <name>`
    GetPool { pool_name: String },
    /// `pools create <name> --class <kind> --durability <kind> [...]`
    CreatePool {
        pool_name: String,
        device_class: String,
        durability_kind: String,
        replication_copies: u32,
        ec_data_shards: u32,
        ec_parity_shards: u32,
        initial_capacity_bytes: u64,
    },
    /// `pools set-durability <name> --durability <kind> [...]`
    SetPoolDurability {
        pool_name: String,
        durability_kind: String,
        replication_copies: u32,
        ec_data_shards: u32,
        ec_parity_shards: u32,
    },
    /// `pools set-thresholds <name> [--warn N] [--critical N] [--readonly N] [--target N]`
    SetPoolThresholds {
        pool_name: String,
        warning_threshold_pct: u32,
        critical_threshold_pct: u32,
        readonly_threshold_pct: u32,
        target_fill_pct: u32,
    },
    /// `pools rebalance <name> [--throughput <mb/s>]`
    RebalancePool {
        pool_name: String,
        throughput_mb_s: u64,
    },
    /// `pools status <name>`
    PoolStatus { pool_name: String },
    /// `tuning get`
    GetTuningParams,
    /// `tuning set <key>=<value>...`
    SetTuningParams { kv: HashMap<String, u32> },
    /// `cluster status`
    ClusterStatus,
    /// `device-health [--device <id>]`
    DeviceHealth { device_id: String },
    /// `io-stats [--pool <name>]`
    IoStats { pool_name: String },
    /// `shards list [--tenant <id>]`
    ListShards { tenant_id: String },
    /// `shards get <id>`
    GetShard { shard_id: String },
    /// `shards split <id>`
    SplitShard { shard_id: String, pivot_key: String },
    /// `shards merge <left> <right>`
    MergeShards {
        left_shard_id: String,
        right_shard_id: String,
    },
    /// `shards maintenance <id> on|off`
    SetShardMaintenance { shard_id: String, enabled: bool },
    /// `scrub [--pool <name>]`
    TriggerScrub { pool_name: String },
    /// `repair-chunk <chunk-id-hex>`
    RepairChunk { chunk_id_hex: String },
    /// `repairs list [--limit N]`
    ListRepairs { limit: u32 },
}

/// Parse `argv` (without the binary name) into a [`StorageAdminCmd`].
/// Returns `Err` with a human-readable usage hint on a malformed
/// invocation. `Ok(None)` is reserved for "show top-level help".
pub fn parse_storage_admin_args(args: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let head = args[0].as_str();
    let rest: &[String] = &args[1..];
    match head {
        "devices" => parse_devices(rest),
        "pools" => parse_pools(rest),
        "tuning" => parse_tuning(rest),
        "cluster" => parse_cluster(rest),
        "device-health" => Ok(Some(parse_device_health(rest))),
        "io-stats" => Ok(Some(parse_io_stats(rest))),
        "shards" => parse_shards(rest),
        "scrub" => Ok(Some(StorageAdminCmd::TriggerScrub {
            pool_name: parse_named_string(rest, "pool"),
        })),
        "repair-chunk" => {
            let chunk = positional(rest, 0, "<chunk-id-hex>")?;
            Ok(Some(StorageAdminCmd::RepairChunk {
                chunk_id_hex: chunk,
            }))
        }
        "repairs" => parse_repairs(rest),
        "help" | "--help" | "-h" => Ok(None),
        other => Err(format!("unknown verb `{other}`; try `help`")),
    }
}

fn parse_devices(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest
        .first()
        .ok_or("devices requires a subcommand")?
        .as_str();
    let tail = &rest[1..];
    match sub {
        "list" => Ok(Some(StorageAdminCmd::ListDevices {
            pool_name: parse_named_string(tail, "pool"),
        })),
        "get" => Ok(Some(StorageAdminCmd::GetDevice {
            device_id: positional(tail, 0, "<device-id>")?,
        })),
        "add" => {
            let pool_name = positional(tail, 0, "<pool>")?;
            let device_id = positional(tail, 1, "<device-id>")?;
            let flags = &tail[2.min(tail.len())..];
            Ok(Some(StorageAdminCmd::AddDevice {
                pool_name,
                device_id,
                capacity_bytes: parse_named_u64(flags, "capacity"),
                device_class: parse_named_string(flags, "class"),
            }))
        }
        "remove" => Ok(Some(StorageAdminCmd::RemoveDevice {
            device_id: positional(tail, 0, "<device-id>")?,
        })),
        "evacuate" => {
            let device_id = positional(tail, 0, "<device-id>")?;
            Ok(Some(StorageAdminCmd::EvacuateDevice {
                device_id,
                throughput_mb_s: parse_named_u64(&tail[1.min(tail.len())..], "throughput"),
            }))
        }
        "cancel-evacuation" => Ok(Some(StorageAdminCmd::CancelEvacuation {
            evacuation_id: positional(tail, 0, "<evac-id>")?,
        })),
        other => Err(format!("unknown devices subcommand `{other}`")),
    }
}

fn parse_pools(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest.first().ok_or("pools requires a subcommand")?.as_str();
    let tail = &rest[1..];
    match sub {
        "list" => Ok(Some(StorageAdminCmd::ListPools)),
        "get" => Ok(Some(StorageAdminCmd::GetPool {
            pool_name: positional(tail, 0, "<name>")?,
        })),
        "status" => Ok(Some(StorageAdminCmd::PoolStatus {
            pool_name: positional(tail, 0, "<name>")?,
        })),
        "create" => {
            let pool_name = positional(tail, 0, "<name>")?;
            let flags = &tail[1.min(tail.len())..];
            Ok(Some(StorageAdminCmd::CreatePool {
                pool_name,
                device_class: parse_named_string(flags, "class"),
                durability_kind: parse_named_string(flags, "durability"),
                replication_copies: parse_named_u32(flags, "copies"),
                ec_data_shards: parse_named_u32(flags, "data-shards"),
                ec_parity_shards: parse_named_u32(flags, "parity-shards"),
                initial_capacity_bytes: parse_named_u64(flags, "capacity"),
            }))
        }
        "set-durability" => {
            let pool_name = positional(tail, 0, "<name>")?;
            let flags = &tail[1.min(tail.len())..];
            Ok(Some(StorageAdminCmd::SetPoolDurability {
                pool_name,
                durability_kind: parse_named_string(flags, "durability"),
                replication_copies: parse_named_u32(flags, "copies"),
                ec_data_shards: parse_named_u32(flags, "data-shards"),
                ec_parity_shards: parse_named_u32(flags, "parity-shards"),
            }))
        }
        "set-thresholds" => {
            let pool_name = positional(tail, 0, "<name>")?;
            let flags = &tail[1.min(tail.len())..];
            Ok(Some(StorageAdminCmd::SetPoolThresholds {
                pool_name,
                warning_threshold_pct: parse_named_u32(flags, "warn"),
                critical_threshold_pct: parse_named_u32(flags, "critical"),
                readonly_threshold_pct: parse_named_u32(flags, "readonly"),
                target_fill_pct: parse_named_u32(flags, "target"),
            }))
        }
        "rebalance" => {
            let pool_name = positional(tail, 0, "<name>")?;
            Ok(Some(StorageAdminCmd::RebalancePool {
                pool_name,
                throughput_mb_s: parse_named_u64(&tail[1.min(tail.len())..], "throughput"),
            }))
        }
        other => Err(format!("unknown pools subcommand `{other}`")),
    }
}

fn parse_tuning(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest.first().ok_or("tuning requires a subcommand")?.as_str();
    let tail = &rest[1..];
    match sub {
        "get" => Ok(Some(StorageAdminCmd::GetTuningParams)),
        "set" => {
            if tail.is_empty() {
                return Err("tuning set requires at least one key=value pair".to_owned());
            }
            let mut kv = HashMap::new();
            for raw in tail {
                let (k, v) = raw
                    .split_once('=')
                    .ok_or_else(|| format!("expected key=value, got `{raw}`"))?;
                let val = v
                    .parse::<u32>()
                    .map_err(|e| format!("`{k}` value `{v}` not a u32: {e}"))?;
                kv.insert(k.to_owned(), val);
            }
            Ok(Some(StorageAdminCmd::SetTuningParams { kv }))
        }
        other => Err(format!("unknown tuning subcommand `{other}`")),
    }
}

fn parse_cluster(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest
        .first()
        .ok_or("cluster requires a subcommand")?
        .as_str();
    match sub {
        "status" => Ok(Some(StorageAdminCmd::ClusterStatus)),
        other => Err(format!("unknown cluster subcommand `{other}`")),
    }
}

fn parse_device_health(rest: &[String]) -> StorageAdminCmd {
    StorageAdminCmd::DeviceHealth {
        device_id: parse_named_string(rest, "device"),
    }
}

fn parse_io_stats(rest: &[String]) -> StorageAdminCmd {
    StorageAdminCmd::IoStats {
        pool_name: parse_named_string(rest, "pool"),
    }
}

fn parse_shards(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest.first().ok_or("shards requires a subcommand")?.as_str();
    let tail = &rest[1..];
    match sub {
        "list" => Ok(Some(StorageAdminCmd::ListShards {
            tenant_id: parse_named_string(tail, "tenant"),
        })),
        "get" => Ok(Some(StorageAdminCmd::GetShard {
            shard_id: positional(tail, 0, "<id>")?,
        })),
        "split" => {
            let shard_id = positional(tail, 0, "<id>")?;
            Ok(Some(StorageAdminCmd::SplitShard {
                shard_id,
                pivot_key: parse_named_string(&tail[1.min(tail.len())..], "pivot"),
            }))
        }
        "merge" => {
            let left = positional(tail, 0, "<left>")?;
            let right = positional(tail, 1, "<right>")?;
            Ok(Some(StorageAdminCmd::MergeShards {
                left_shard_id: left,
                right_shard_id: right,
            }))
        }
        "maintenance" => {
            let shard_id = positional(tail, 0, "<id>")?;
            let toggle = positional(tail, 1, "on|off")?;
            let enabled = match toggle.as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("expected on|off, got `{other}`")),
            };
            Ok(Some(StorageAdminCmd::SetShardMaintenance {
                shard_id,
                enabled,
            }))
        }
        other => Err(format!("unknown shards subcommand `{other}`")),
    }
}

fn parse_repairs(rest: &[String]) -> Result<Option<StorageAdminCmd>, String> {
    let sub = rest
        .first()
        .ok_or("repairs requires a subcommand")?
        .as_str();
    let tail = &rest[1..];
    match sub {
        "list" => Ok(Some(StorageAdminCmd::ListRepairs {
            limit: parse_named_u32(tail, "limit"),
        })),
        other => Err(format!("unknown repairs subcommand `{other}`")),
    }
}

fn positional(args: &[String], idx: usize, name: &str) -> Result<String, String> {
    args.get(idx)
        .cloned()
        .ok_or_else(|| format!("expected positional {name}"))
}

/// `--name <value>` or `--name=<value>`. Returns "" if absent.
fn parse_named_string(args: &[String], name: &str) -> String {
    let dashed = format!("--{name}");
    let dashed_eq = format!("--{name}=");
    let mut i = 0;
    while i < args.len() {
        if args[i] == dashed {
            return args.get(i + 1).cloned().unwrap_or_default();
        }
        if let Some(rest) = args[i].strip_prefix(&dashed_eq) {
            return rest.to_owned();
        }
        i += 1;
    }
    String::new()
}

fn parse_named_u32(args: &[String], name: &str) -> u32 {
    let s = parse_named_string(args, name);
    s.parse().unwrap_or(0)
}

fn parse_named_u64(args: &[String], name: &str) -> u64 {
    let s = parse_named_string(args, name);
    s.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_args_returns_help() {
        assert_eq!(parse_storage_admin_args(&[]).expect("ok"), None);
    }

    #[test]
    fn help_returns_none() {
        assert_eq!(
            parse_storage_admin_args(&args(&["help"])).expect("ok"),
            None,
        );
    }

    #[test]
    fn unknown_verb_returns_err() {
        assert!(parse_storage_admin_args(&args(&["frobnicate"])).is_err());
    }

    // --- Devices ---

    #[test]
    fn devices_list_no_filter() {
        let r = parse_storage_admin_args(&args(&["devices", "list"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::ListDevices {
                pool_name: String::new(),
            },
        );
    }

    #[test]
    fn devices_list_with_pool_filter() {
        let r = parse_storage_admin_args(&args(&["devices", "list", "--pool", "hot"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::ListDevices {
                pool_name: "hot".into(),
            },
        );
    }

    #[test]
    fn devices_add_extracts_positional_and_flags() {
        let r = parse_storage_admin_args(&args(&[
            "devices",
            "add",
            "hot",
            "dev-7",
            "--capacity",
            "1024",
            "--class",
            "nvme",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::AddDevice {
                pool_name: "hot".into(),
                device_id: "dev-7".into(),
                capacity_bytes: 1024,
                device_class: "nvme".into(),
            },
        );
    }

    #[test]
    fn devices_get_requires_id() {
        assert!(parse_storage_admin_args(&args(&["devices", "get"])).is_err());
    }

    #[test]
    fn devices_evacuate_with_throughput() {
        let r = parse_storage_admin_args(&args(&[
            "devices",
            "evacuate",
            "dev-7",
            "--throughput",
            "100",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::EvacuateDevice {
                device_id: "dev-7".into(),
                throughput_mb_s: 100,
            },
        );
    }

    #[test]
    fn devices_cancel_evacuation() {
        let r = parse_storage_admin_args(&args(&["devices", "cancel-evacuation", "evac-1"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::CancelEvacuation {
                evacuation_id: "evac-1".into(),
            },
        );
    }

    // --- Pools ---

    #[test]
    fn pools_list() {
        let r = parse_storage_admin_args(&args(&["pools", "list"]))
            .unwrap()
            .unwrap();
        assert_eq!(r, StorageAdminCmd::ListPools);
    }

    #[test]
    fn pools_create_full() {
        let r = parse_storage_admin_args(&args(&[
            "pools",
            "create",
            "hot",
            "--class",
            "nvme",
            "--durability",
            "erasure_coding",
            "--data-shards",
            "4",
            "--parity-shards",
            "2",
            "--capacity",
            "1000000",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::CreatePool {
                pool_name: "hot".into(),
                device_class: "nvme".into(),
                durability_kind: "erasure_coding".into(),
                replication_copies: 0,
                ec_data_shards: 4,
                ec_parity_shards: 2,
                initial_capacity_bytes: 1_000_000,
            },
        );
    }

    #[test]
    fn pools_set_thresholds() {
        let r = parse_storage_admin_args(&args(&[
            "pools",
            "set-thresholds",
            "hot",
            "--warn",
            "60",
            "--critical",
            "80",
            "--readonly",
            "90",
            "--target",
            "70",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::SetPoolThresholds {
                pool_name: "hot".into(),
                warning_threshold_pct: 60,
                critical_threshold_pct: 80,
                readonly_threshold_pct: 90,
                target_fill_pct: 70,
            },
        );
    }

    #[test]
    fn pools_rebalance_default_throughput() {
        let r = parse_storage_admin_args(&args(&["pools", "rebalance", "hot"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::RebalancePool {
                pool_name: "hot".into(),
                throughput_mb_s: 0,
            },
        );
    }

    // --- Tuning ---

    #[test]
    fn tuning_get() {
        let r = parse_storage_admin_args(&args(&["tuning", "get"]))
            .unwrap()
            .unwrap();
        assert_eq!(r, StorageAdminCmd::GetTuningParams);
    }

    #[test]
    fn tuning_set_kv_pairs() {
        let r = parse_storage_admin_args(&args(&[
            "tuning",
            "set",
            "compaction_rate_mb_s=200",
            "scrub_interval_h=48",
        ]))
        .unwrap()
        .unwrap();
        match r {
            StorageAdminCmd::SetTuningParams { kv } => {
                assert_eq!(kv.get("compaction_rate_mb_s"), Some(&200));
                assert_eq!(kv.get("scrub_interval_h"), Some(&48));
            }
            other => panic!("expected SetTuningParams, got {other:?}"),
        }
    }

    #[test]
    fn tuning_set_requires_kv() {
        assert!(parse_storage_admin_args(&args(&["tuning", "set"])).is_err());
    }

    #[test]
    fn tuning_set_rejects_non_kv_arg() {
        assert!(parse_storage_admin_args(&args(&["tuning", "set", "no-equals"])).is_err());
    }

    // --- Cluster + observability ---

    #[test]
    fn cluster_status() {
        let r = parse_storage_admin_args(&args(&["cluster", "status"]))
            .unwrap()
            .unwrap();
        assert_eq!(r, StorageAdminCmd::ClusterStatus);
    }

    #[test]
    fn device_health_no_filter() {
        let r = parse_storage_admin_args(&args(&["device-health"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::DeviceHealth {
                device_id: String::new(),
            },
        );
    }

    #[test]
    fn io_stats_with_pool_filter() {
        let r = parse_storage_admin_args(&args(&["io-stats", "--pool", "hot"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::IoStats {
                pool_name: "hot".into(),
            },
        );
    }

    // --- Shards ---

    #[test]
    fn shards_list_with_tenant_filter() {
        let r = parse_storage_admin_args(&args(&["shards", "list", "--tenant", "abc"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::ListShards {
                tenant_id: "abc".into(),
            },
        );
    }

    #[test]
    fn shards_split() {
        let r = parse_storage_admin_args(&args(&["shards", "split", "shard-1"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::SplitShard {
                shard_id: "shard-1".into(),
                pivot_key: String::new(),
            },
        );
    }

    #[test]
    fn shards_merge() {
        let r = parse_storage_admin_args(&args(&["shards", "merge", "left", "right"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::MergeShards {
                left_shard_id: "left".into(),
                right_shard_id: "right".into(),
            },
        );
    }

    #[test]
    fn shards_maintenance_on() {
        let r = parse_storage_admin_args(&args(&["shards", "maintenance", "shard-1", "on"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::SetShardMaintenance {
                shard_id: "shard-1".into(),
                enabled: true,
            },
        );
    }

    #[test]
    fn shards_maintenance_rejects_invalid_toggle() {
        assert!(
            parse_storage_admin_args(&args(&["shards", "maintenance", "shard-1", "maybe",]))
                .is_err()
        );
    }

    // --- Repair / scrub ---

    #[test]
    fn scrub_no_pool() {
        let r = parse_storage_admin_args(&args(&["scrub"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::TriggerScrub {
                pool_name: String::new(),
            },
        );
    }

    #[test]
    fn scrub_with_pool() {
        let r = parse_storage_admin_args(&args(&["scrub", "--pool", "hot"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::TriggerScrub {
                pool_name: "hot".into(),
            },
        );
    }

    #[test]
    fn repair_chunk() {
        let r = parse_storage_admin_args(&args(&["repair-chunk", "deadbeef"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r,
            StorageAdminCmd::RepairChunk {
                chunk_id_hex: "deadbeef".into(),
            },
        );
    }

    #[test]
    fn repairs_list_with_limit() {
        let r = parse_storage_admin_args(&args(&["repairs", "list", "--limit", "50"]))
            .unwrap()
            .unwrap();
        assert_eq!(r, StorageAdminCmd::ListRepairs { limit: 50 });
    }

    // --- Cardinality cross-check ---

    /// Mechanical guard: the parser must produce a distinct
    /// `StorageAdminCmd` variant for each of the 26 RPCs.
    #[test]
    fn cli_covers_all_26_rpcs() {
        // Map each RPC verb to a parseable invocation and assert
        // we produce a valid variant for it.
        let invocations: &[&[&str]] = &[
            &["devices", "list"],
            &["devices", "get", "x"],
            &["devices", "add", "p", "d"],
            &["devices", "remove", "x"],
            &["devices", "evacuate", "x"],
            &["devices", "cancel-evacuation", "x"],
            &["pools", "list"],
            &["pools", "get", "x"],
            &["pools", "create", "x"],
            &["pools", "set-durability", "x"],
            &["pools", "set-thresholds", "x"],
            &["pools", "rebalance", "x"],
            &["pools", "status", "x"],
            &["tuning", "get"],
            &["tuning", "set", "k=1"],
            &["cluster", "status"],
            &["device-health"],
            &["io-stats"],
            &["shards", "list"],
            &["shards", "get", "x"],
            &["shards", "split", "x"],
            &["shards", "merge", "l", "r"],
            &["shards", "maintenance", "x", "on"],
            &["scrub"],
            &["repair-chunk", "x"],
            &["repairs", "list"],
        ];
        assert_eq!(
            invocations.len(),
            26,
            "must enumerate exactly 26 RPC invocations",
        );
        for parts in invocations {
            let v = args(parts);
            let parsed = parse_storage_admin_args(&v);
            assert!(parsed.is_ok(), "parse failed for {parts:?}: {parsed:?}");
            assert!(
                parsed.unwrap().is_some(),
                "no command produced for {parts:?}",
            );
        }
    }
}
