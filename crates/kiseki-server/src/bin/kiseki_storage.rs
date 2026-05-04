#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! `kiseki-storage` — gRPC client for the `StorageAdminService`
//! (ADR-025 W6). 26 verbs covering every RPC.
//!
//! Endpoint defaults to `KISEKI_STORAGE_ENDPOINT` env var, or
//! `http://localhost:50051` (the standard data-path gRPC port).
//! Override with `--endpoint <url>`.

use std::fmt::Write as _;
use std::process::ExitCode;

use kiseki_proto::v1 as pb;
use kiseki_proto::v1::storage_admin_service_client::StorageAdminServiceClient;
use tonic::transport::Channel;

// Reach into the shared parser module without making the server
// crate a library — the binary just inlines it via `#[path]`.
#[path = "../storage_admin_cli.rs"]
mod storage_admin_cli;
use storage_admin_cli::{parse_storage_admin_args, StorageAdminCmd};

const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

fn print_usage() {
    eprintln!(
        "kiseki-storage -- StorageAdminService gRPC client (ADR-025)\n\
         \n\
         Usage:\n\
         \x20 kiseki-storage [--endpoint URL] <verb> [args...]\n\
         \n\
         Verbs (26 total — one per StorageAdminService RPC):\n\
         \n\
         Devices:\n\
         \x20 devices list [--pool NAME]\n\
         \x20 devices get DEVICE_ID\n\
         \x20 devices add POOL DEVICE_ID [--capacity BYTES] [--class KIND]\n\
         \x20 devices remove DEVICE_ID\n\
         \x20 devices evacuate DEVICE_ID [--throughput MB/S]\n\
         \x20 devices cancel-evacuation EVAC_ID\n\
         \n\
         Pools:\n\
         \x20 pools list\n\
         \x20 pools get NAME\n\
         \x20 pools status NAME\n\
         \x20 pools create NAME --class KIND --durability KIND [--copies N | --data-shards N --parity-shards N] [--capacity BYTES]\n\
         \x20 pools set-durability NAME --durability KIND [--copies N | --data-shards N --parity-shards N]\n\
         \x20 pools set-thresholds NAME [--warn N] [--critical N] [--readonly N] [--target N]\n\
         \x20 pools rebalance NAME [--throughput MB/S]\n\
         \n\
         Tuning + cluster:\n\
         \x20 tuning get\n\
         \x20 tuning set KEY=VALUE [KEY=VALUE...]\n\
         \x20 cluster status\n\
         \n\
         Observability streams:\n\
         \x20 device-health [--device ID]\n\
         \x20 io-stats [--pool NAME]\n\
         \n\
         Shards:\n\
         \x20 shards list [--tenant ID]\n\
         \x20 shards get ID\n\
         \x20 shards split ID [--pivot KEY]\n\
         \x20 shards merge LEFT_ID RIGHT_ID\n\
         \x20 shards maintenance ID on|off\n\
         \n\
         Repair:\n\
         \x20 scrub [--pool NAME]\n\
         \x20 repair-chunk CHUNK_ID_HEX\n\
         \x20 repairs list [--limit N]\n\
         \n\
         Endpoint defaults to KISEKI_STORAGE_ENDPOINT env var, or http://localhost:50051"
    );
}

fn default_endpoint() -> String {
    std::env::var("KISEKI_STORAGE_ENDPOINT").unwrap_or_else(|_| "http://localhost:50051".to_owned())
}

/// Strip the `--endpoint <url>` (or `--endpoint=<url>`) prefix and
/// return `(endpoint, remaining_args)`.
fn extract_endpoint(args: &[String]) -> (String, Vec<String>) {
    let mut i = 0;
    let mut endpoint = None;
    while i < args.len() {
        if args[i] == "--endpoint" {
            i += 1;
            endpoint = args.get(i).cloned();
            i += 1;
        } else if let Some(rest) = args[i].strip_prefix("--endpoint=") {
            endpoint = Some(rest.to_owned());
            i += 1;
        } else {
            break;
        }
    }
    (
        endpoint.unwrap_or_else(default_endpoint),
        args[i..].to_vec(),
    )
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        print_usage();
        return ExitCode::from(0);
    }
    let (endpoint, rest) = extract_endpoint(&raw);
    let cmd = match parse_storage_admin_args(&rest) {
        Ok(Some(c)) => c,
        Ok(None) => {
            print_usage();
            return ExitCode::from(0);
        }
        Err(e) => {
            eprintln!("{RED}error{RESET}: {e}");
            print_usage();
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{RED}error{RESET}: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(endpoint, cmd)) {
        Ok(out) => {
            print!("{out}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("{RED}error{RESET}: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run(endpoint: String, cmd: StorageAdminCmd) -> Result<String, String> {
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| format!("invalid endpoint `{endpoint}`: {e}"))?
        .connect()
        .await
        .map_err(|e| format!("connect to {endpoint}: {e}"))?;
    let mut client = StorageAdminServiceClient::new(channel);
    dispatch(&mut client, cmd).await
}

#[allow(clippy::too_many_lines)] // 26 RPC arms — each is small but additive
async fn dispatch(
    client: &mut StorageAdminServiceClient<Channel>,
    cmd: StorageAdminCmd,
) -> Result<String, String> {
    match cmd {
        StorageAdminCmd::ListDevices { pool_name } => {
            let r = client
                .list_devices(pb::ListDevicesRequest { pool_name })
                .await
                .map_err(format_status)?
                .into_inner();
            let mut s = format!("{} device(s)\n", r.devices.len());
            for d in &r.devices {
                let _ = writeln!(
                    s,
                    "  {} pool={} class={} {}/{} bytes online={}",
                    d.device_id,
                    d.pool_name,
                    d.device_class,
                    d.used_bytes,
                    d.capacity_bytes,
                    d.online,
                );
            }
            Ok(s)
        }
        StorageAdminCmd::GetDevice { device_id } => {
            let r = client
                .get_device(pb::GetDeviceRequest { device_id })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::AddDevice {
            pool_name,
            device_id,
            capacity_bytes,
            device_class,
        } => {
            let r = client
                .add_device(pb::AddDeviceRequest {
                    pool_name,
                    device_id,
                    capacity_bytes,
                    device_class,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "device added (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::RemoveDevice { device_id } => {
            let r = client
                .remove_device(pb::RemoveDeviceRequest { device_id })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "device removed (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::EvacuateDevice {
            device_id,
            throughput_mb_s,
        } => {
            let r = client
                .evacuate_device(pb::EvacuateDeviceRequest {
                    device_id,
                    throughput_mb_s,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "evacuation started (id={}, log_index={})\n",
                r.evacuation_id, r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::CancelEvacuation { evacuation_id } => {
            let r = client
                .cancel_evacuation(pb::CancelEvacuationRequest { evacuation_id })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "evacuation cancelled (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::ListPools => {
            let r = client
                .list_pools(pb::ListPoolsRequest {})
                .await
                .map_err(format_status)?
                .into_inner();
            let mut s = format!("{} pool(s)\n", r.pools.len());
            for p in &r.pools {
                let _ = writeln!(
                    s,
                    "  {} durability={} replication={} ec={}+{} {}/{} bytes devices={}",
                    p.pool_name,
                    p.durability_kind,
                    p.replication_copies,
                    p.ec_data_shards,
                    p.ec_parity_shards,
                    p.used_bytes,
                    p.capacity_bytes,
                    p.device_count,
                );
            }
            Ok(s)
        }
        StorageAdminCmd::GetPool { pool_name } => {
            let r = client
                .get_pool(pb::GetPoolRequest { pool_name })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::PoolStatus { pool_name } => {
            let r = client
                .pool_status(pb::PoolStatusRequest { pool_name })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::CreatePool {
            pool_name,
            device_class,
            durability_kind,
            replication_copies,
            ec_data_shards,
            ec_parity_shards,
            initial_capacity_bytes,
        } => {
            let r = client
                .create_pool(pb::CreatePoolRequest {
                    pool_name,
                    device_class,
                    durability_kind,
                    replication_copies,
                    ec_data_shards,
                    ec_parity_shards,
                    initial_capacity_bytes,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "pool created (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::SetPoolDurability {
            pool_name,
            durability_kind,
            replication_copies,
            ec_data_shards,
            ec_parity_shards,
        } => {
            let r = client
                .set_pool_durability(pb::SetPoolDurabilityRequest {
                    pool_name,
                    durability_kind,
                    replication_copies,
                    ec_data_shards,
                    ec_parity_shards,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "pool durability updated (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::SetPoolThresholds {
            pool_name,
            warning_threshold_pct,
            critical_threshold_pct,
            readonly_threshold_pct,
            target_fill_pct,
        } => {
            let r = client
                .set_pool_thresholds(pb::SetPoolThresholdsRequest {
                    pool_name,
                    warning_threshold_pct,
                    critical_threshold_pct,
                    readonly_threshold_pct,
                    target_fill_pct,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "pool thresholds updated (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::RebalancePool {
            pool_name,
            throughput_mb_s,
        } => {
            let r = client
                .rebalance_pool(pb::RebalancePoolRequest {
                    pool_name,
                    throughput_mb_s,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("rebalance triggered: id={}\n", r.rebalance_id))
        }
        StorageAdminCmd::GetTuningParams => {
            let r = client
                .get_tuning_params(pb::GetTuningParamsRequest {})
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::SetTuningParams { kv } => {
            // Read current, apply overrides, write back. Lets the
            // user set just one field without filling out all 8.
            let current = client
                .get_tuning_params(pb::GetTuningParamsRequest {})
                .await
                .map_err(format_status)?
                .into_inner();
            let mut params = current;
            for (k, v) in &kv {
                match k.as_str() {
                    "compaction_rate_mb_s" => params.compaction_rate_mb_s = *v,
                    "gc_interval_s" => params.gc_interval_s = *v,
                    "rebalance_rate_mb_s" => params.rebalance_rate_mb_s = *v,
                    "scrub_interval_h" => params.scrub_interval_h = *v,
                    "max_concurrent_repairs" => params.max_concurrent_repairs = *v,
                    "stream_proc_poll_ms" => params.stream_proc_poll_ms = *v,
                    "inline_threshold_bytes" => params.inline_threshold_bytes = *v,
                    "raft_snapshot_interval" => params.raft_snapshot_interval = *v,
                    other => return Err(format!("unknown tuning param `{other}`")),
                }
            }
            let r = client
                .set_tuning_params(pb::SetTuningParamsRequest {
                    params: Some(params),
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "tuning params updated (committed at log index {})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::ClusterStatus => {
            let r = client
                .cluster_status(pb::ClusterStatusRequest {})
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::DeviceHealth { device_id } => {
            // Streaming RPC; consume the first event then stop —
            // operators can re-run for the next event. A `tail`
            // mode lands when the stream is multi-shot.
            let mut stream = client
                .device_health(pb::DeviceHealthRequest { device_id })
                .await
                .map_err(format_status)?
                .into_inner();
            match stream.message().await.map_err(format_status)? {
                Some(ev) => Ok(format!("{ev:#?}\n")),
                None => Ok("(no events)\n".to_owned()),
            }
        }
        StorageAdminCmd::IoStats { pool_name } => {
            // The proto's IOStatsRequest only carries
            // `sample_interval_ms`; the CLI's `--pool` filter is
            // accepted today but ignored at the wire (server-side
            // filtering lands with W7's full streaming impl).
            let _ = pool_name;
            let mut stream = client
                .io_stats(pb::IoStatsRequest {
                    sample_interval_ms: 1000,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            match stream.message().await.map_err(format_status)? {
                Some(ev) => Ok(format!("{ev:#?}\n")),
                None => Ok("(no events)\n".to_owned()),
            }
        }
        StorageAdminCmd::ListShards { tenant_id } => {
            let r = client
                .list_shards(pb::ListShardsRequest { tenant_id })
                .await
                .map_err(format_status)?
                .into_inner();
            let mut s = format!("{} shard(s)\n", r.shards.len());
            for sh in &r.shards {
                let _ = writeln!(
                    s,
                    "  {} tenant={} leader={} members={} maintenance={}",
                    sh.shard_id,
                    sh.tenant_id,
                    sh.leader_node,
                    sh.members.join(","),
                    sh.maintenance,
                );
            }
            Ok(s)
        }
        StorageAdminCmd::GetShard { shard_id } => {
            let r = client
                .get_shard(pb::GetShardRequest { shard_id })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("{r:#?}\n"))
        }
        StorageAdminCmd::SplitShard {
            shard_id,
            pivot_key,
        } => {
            let r = client
                .split_shard(pb::SplitShardRequest {
                    shard_id,
                    pivot_key,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "shard split: left={} right={} (log_index={})\n",
                r.left_shard_id, r.right_shard_id, r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::MergeShards {
            left_shard_id,
            right_shard_id,
        } => {
            let r = client
                .merge_shards(pb::MergeShardsRequest {
                    left_shard_id,
                    right_shard_id,
                })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "shards merged: id={} (log_index={})\n",
                r.merged_shard_id, r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::SetShardMaintenance { shard_id, enabled } => {
            let r = client
                .set_shard_maintenance(pb::SetShardMaintenanceRequest { shard_id, enabled })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "shard maintenance updated (log_index={})\n",
                r.committed_at_log_index,
            ))
        }
        StorageAdminCmd::TriggerScrub { pool_name } => {
            let r = client
                .trigger_scrub(pb::TriggerScrubRequest { pool_name })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!("scrub triggered: id={}\n", r.scrub_id))
        }
        StorageAdminCmd::RepairChunk { chunk_id_hex } => {
            let r = client
                .repair_chunk(pb::AdminRepairChunkRequest { chunk_id_hex })
                .await
                .map_err(format_status)?
                .into_inner();
            Ok(format!(
                "repair triggered: id={} already_healthy={}\n",
                r.repair_id, r.already_healthy,
            ))
        }
        StorageAdminCmd::ListRepairs { limit } => {
            let r = client
                .list_repairs(pb::ListRepairsRequest { limit })
                .await
                .map_err(format_status)?
                .into_inner();
            let mut s = format!("{} repair record(s)\n", r.repairs.len());
            for rr in &r.repairs {
                let _ = writeln!(
                    s,
                    "  {} chunk={} {} {} {}",
                    rr.repair_id, rr.chunk_id_hex, rr.trigger, rr.state, rr.detail,
                );
            }
            Ok(s)
        }
    }
}

/// `.map_err`-friendly helper. Takes `tonic::Status` by value
/// because that's the err shape coming out of tonic's RPC calls.
#[allow(clippy::needless_pass_by_value)]
fn format_status(s: tonic::Status) -> String {
    format!("{}: {}", s.code(), s.message())
}
