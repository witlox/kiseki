#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(
    clippy::assigning_clones,
    clippy::too_many_lines,
    clippy::map_unwrap_or,
    clippy::useless_format,
    clippy::write_literal
)]
//! kiseki-admin -- remote cluster administration CLI.
//!
//! Connects to any Kiseki node via the REST API at `:9090`.
//!
//! Default endpoint: `localhost:9090` (or `KISEKI_ENDPOINT` env var).

use std::fmt::Write as FmtWrite;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// ---------------------------------------------------------------------------
// ANSI colour helpers
// ---------------------------------------------------------------------------
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

// ---------------------------------------------------------------------------
// HTTP helpers (raw TCP, no external crate)
// ---------------------------------------------------------------------------

/// Extract `host:port` from an `http://host:port/...` URL.
fn extract_host_port(url: &str) -> Option<String> {
    url.strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .map(String::from)
}

/// Read an HTTP response from a connected stream and return the body.
fn read_http_body(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;

    let text = String::from_utf8_lossy(&buf);
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed HTTP response")?;

    let body = &text[body_start..];
    if text[..body_start]
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        Ok(decode_chunked(body))
    } else {
        Ok(body.to_string())
    }
}

/// Perform a blocking HTTP GET, return the response body.
fn http_get(endpoint: &str, path: &str) -> Result<String, String> {
    let url = format!("{endpoint}{path}");
    let host_port = extract_host_port(&url).ok_or("invalid endpoint URL")?;

    let mut stream = TcpStream::connect(&host_port)
        .map_err(|e| format!("connection failed ({host_port}): {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.flush().map_err(|e| format!("flush failed: {e}"))?;

    read_http_body(&mut stream)
}

/// Perform a blocking HTTP POST with a JSON body, return the response body.
fn http_post(endpoint: &str, path: &str, body: &str) -> Result<String, String> {
    let url = format!("{endpoint}{path}");
    let host_port = extract_host_port(&url).ok_or("invalid endpoint URL")?;

    let mut stream = TcpStream::connect(&host_port)
        .map_err(|e| format!("connection failed ({host_port}): {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.flush().map_err(|e| format!("flush failed: {e}"))?;

    read_http_body(&mut stream)
}

/// Decode a chunked transfer-encoding body.
fn decode_chunked(input: &str) -> String {
    let mut result = String::new();
    let mut remaining = input;
    loop {
        let remaining_trimmed = remaining.trim_start();
        if remaining_trimmed.is_empty() {
            break;
        }
        let line_end = remaining_trimmed
            .find("\r\n")
            .unwrap_or(remaining_trimmed.len());
        let size_str = &remaining_trimmed[..line_end];
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let data_start = line_end + 2;
        if data_start + size <= remaining_trimmed.len() {
            result.push_str(&remaining_trimmed[data_start..data_start + size]);
            remaining = &remaining_trimmed[data_start + size..];
            if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
        } else {
            result.push_str(&remaining_trimmed[data_start..]);
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Minimal JSON helpers (no serde -- this binary uses only std)
// ---------------------------------------------------------------------------

/// Extract a string value for a given key from a JSON object.
fn json_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    let stripped = after_ws.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(&stripped[..end])
}

/// Extract a numeric value (u64) for a given key.
fn json_u64(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    let end = after_ws
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(after_ws.len());
    let num_str = &after_ws[..end];
    if let Some(dot) = num_str.find('.') {
        num_str[..dot].parse().ok()
    } else {
        num_str.parse().ok()
    }
}

/// Extract a signed numeric value (i64) for a given key.
fn json_i64(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    let end = after_ws
        .find(|c: char| !c.is_ascii_digit() && c != '-' && c != '.')
        .unwrap_or(after_ws.len());
    let num_str = &after_ws[..end];
    if let Some(dot) = num_str.find('.') {
        num_str[..dot].parse().ok()
    } else {
        num_str.parse().ok()
    }
}

/// Extract a boolean value for a given key.
fn json_bool(json: &str, key: &str) -> Option<bool> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    if after_ws.starts_with("true") {
        Some(true)
    } else if after_ws.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Split a JSON array (`[...]`) into individual object strings.
fn json_array_elements(json: &str) -> Vec<&str> {
    let trimmed = json.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        return Vec::new();
    };

    let mut elements = Vec::new();
    let mut depth = 0i32;
    let mut start = None;

    for (i, c) in inner.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        elements.push(&inner[s..=i]);
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    elements
}

/// Extract the JSON array value for a given key.
fn json_array_value<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    if !after_ws.starts_with('[') {
        return None;
    }
    let mut depth = 0i32;
    for (i, c) in after_ws.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&after_ws[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_099_511_627_776 {
        format!("{:.1} TB", bytes as f64 / 1_099_511_627_776.0)
    } else if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Extract `HH:MM:SS` from an ISO timestamp, or return the input as-is.
fn shorten_timestamp(time: &str) -> &str {
    if let Some(t_pos) = time.find('T') {
        let after_t = &time[t_pos + 1..];
        &after_t[..after_t.len().min(8)]
    } else if time.len() > 8 {
        &time[..8]
    } else {
        time
    }
}

fn format_cluster_status(body: &str) -> String {
    let total = json_u64(body, "total_nodes").unwrap_or(0);
    let healthy = json_u64(body, "healthy_nodes").unwrap_or(0);

    let agg_start = body.find("\"aggregate\"").unwrap_or(0);
    let agg = &body[agg_start..];

    let raft = json_u64(agg, "raft_entries").unwrap_or(0);
    let requests = json_u64(agg, "gateway_requests").unwrap_or(0);
    let written = json_u64(agg, "chunk_write_bytes").unwrap_or(0);
    let read = json_u64(agg, "chunk_read_bytes").unwrap_or(0);
    let conns = json_i64(agg, "transport_connections").unwrap_or(0);

    let health_color = if healthy == total { GREEN } else { RED };

    format!(
        "\n{BOLD}Cluster Status{RESET}\n\
         \u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\n\
         Nodes:       {health_color}{}/{}{RESET}\n\
         Raft:        {} entries\n\
         Requests:    {} served\n\
         Written:     {}\n\
         Read:        {}\n\
         Connections: {} active\n",
        healthy,
        total,
        format_number(raft),
        format_number(requests),
        format_bytes(written),
        format_bytes(read),
        conns,
    )
}

fn format_nodes(body: &str) -> String {
    let nodes = json_array_elements(body);
    if nodes.is_empty() {
        return "No nodes found.\n".to_string();
    }

    let mut out = format!(
        "\n{BOLD}{:<18}{:<10}{:<10}{:<10}{:<10}{:<10}{:<6}{RESET}\n",
        "NODE", "STATUS", "RAFT", "REQUESTS", "WRITTEN", "READ", "CONNS"
    );

    for node in &nodes {
        let addr = json_str(node, "address").unwrap_or("?");
        let healthy = json_bool(node, "healthy").unwrap_or(false);

        let sum_start = node.find("\"summary\"").unwrap_or(0);
        let sum = &node[sum_start..];

        let raft = json_u64(sum, "raft_entries").unwrap_or(0);
        let requests = json_u64(sum, "gateway_requests").unwrap_or(0);
        let written = json_u64(sum, "chunk_write_bytes").unwrap_or(0);
        let read = json_u64(sum, "chunk_read_bytes").unwrap_or(0);
        let conns = json_i64(sum, "transport_connections").unwrap_or(0);

        let (status, color) = if healthy {
            ("healthy", GREEN)
        } else {
            ("down", RED)
        };

        let _ = writeln!(
            out,
            "{:<18}{color}{:<10}{RESET}{:<10}{:<10}{:<10}{:<10}{:<6}",
            addr,
            status,
            format_number(raft),
            format_number(requests),
            format_bytes(written),
            format_bytes(read),
            conns,
        );
    }
    out
}

fn format_events(body: &str) -> String {
    let events_arr = json_array_value(body, "events").unwrap_or("[]");
    let events = json_array_elements(events_arr);

    if events.is_empty() {
        return "No events found.\n".to_string();
    }

    let count = json_u64(body, "count").unwrap_or(events.len() as u64);

    let mut out = format!(
        "\nEvents ({count} total)\n\
         {BOLD}{:<10}{:<10}{:<12}{:<12}{}{RESET}\n",
        "TIME", "SEVERITY", "CATEGORY", "SOURCE", "MESSAGE"
    );

    for ev in &events {
        let time = json_str(ev, "time").unwrap_or("?");
        let severity = json_str(ev, "severity").unwrap_or("info");
        let category = json_str(ev, "category").unwrap_or("?");
        let source = json_str(ev, "source").unwrap_or("?");
        let message = json_str(ev, "message").unwrap_or("");

        let color = match severity.to_ascii_lowercase().as_str() {
            "critical" | "error" => RED,
            "warning" => YELLOW,
            _ => GREEN,
        };

        let time_short = shorten_timestamp(time);

        let _ = writeln!(
            out,
            "{:<10}{color}{:<10}{RESET}{:<12}{:<12}{}",
            time_short,
            severity.to_ascii_uppercase(),
            category,
            source,
            message,
        );
    }
    out
}

fn format_history(body: &str) -> String {
    let hours = json_u64(body, "hours").unwrap_or(3);
    let points_arr = json_array_value(body, "points").unwrap_or("[]");
    let points = json_array_elements(points_arr);

    if points.is_empty() {
        return format!("No history data (last {hours} hours).\n");
    }

    let mut out = format!(
        "\n{BOLD}Metric History ({hours}h){RESET}\n\
         {BOLD}{:<12}{:<10}{:<10}{:<10}{:<10}{:<10}{:<6}{RESET}\n",
        "TIME", "RAFT", "REQUESTS", "WRITTEN", "READ", "CONNS", "DELTAS"
    );

    for pt in &points {
        let time = json_str(pt, "time").unwrap_or("?");
        let raft = json_u64(pt, "raft_entries").unwrap_or(0);
        let requests = json_u64(pt, "gateway_requests").unwrap_or(0);
        let written = json_u64(pt, "chunk_write_bytes").unwrap_or(0);
        let read = json_u64(pt, "chunk_read_bytes").unwrap_or(0);
        let conns = json_i64(pt, "transport_connections").unwrap_or(0);
        let deltas = json_u64(pt, "shard_deltas").unwrap_or(0);

        let time_short = shorten_timestamp(time);

        let _ = writeln!(
            out,
            "{:<12}{:<10}{:<10}{:<10}{:<10}{:<10}{:<6}",
            time_short,
            format_number(raft),
            format_number(requests),
            format_bytes(written),
            format_bytes(read),
            conns,
            format_number(deltas),
        );
    }
    out
}

fn format_ops_response(body: &str) -> String {
    let status = json_str(body, "status").unwrap_or("unknown");
    let message = json_str(body, "message").unwrap_or("(no message)");
    let color = if status == "ok" { GREEN } else { RED };
    format!(
        "{color}{}{RESET}: {}\n",
        status.to_ascii_uppercase(),
        message
    )
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct Args {
    endpoint: String,
    json: bool,
    command: Command,
}

enum Command {
    Status,
    Nodes,
    Events {
        severity: Option<String>,
        hours: Option<f64>,
    },
    History {
        hours: Option<f64>,
    },
    Maintenance {
        enabled: bool,
    },
    Backup,
    Scrub,
    Help,
    /// `--version` short-circuit.
    Version,
    /// `shards` — per-shard leader map (mirrors `/cluster/info` shards).
    Shards,
    /// `forwarding` — proxy + stale-leader counters per node.
    Forwarding,
    /// `audit query [--tenant T] [--type X] [--limit N] [--from S] [--local-only]`
    AuditQuery {
        tenant: Option<String>,
        event_type: Option<String>,
        limit: Option<usize>,
        from: Option<u64>,
        /// When true, query only the local node's audit shard.
        /// Default (false) fans out to all peers via the aggregating
        /// endpoint. See ADR-009 + 2026-05-15 follow-ups doc D5.
        local_only: bool,
    },
    /// `tenant <subcmd>`
    TenantCreateOrg {
        name: String,
    },
    TenantCreateProject {
        org_id: String,
        name: String,
    },
    TenantCreateWorkload {
        project_id: String,
        name: String,
    },
    TenantCreateNamespace {
        workload_id: String,
        name: String,
    },
    TenantDescribe {
        id: String,
    },
    TenantDelete {
        id: String,
        yes: bool,
    },
    TenantList {
        kind: String,
    },
    /// `snapshot {create|list|restore}`
    SnapshotCreate {
        note: Option<String>,
    },
    SnapshotList,
    SnapshotRestore {
        snapshot_id: String,
    },
    /// `drain <node-id>`
    Drain {
        node_id: u64,
    },
    DrainCancel {
        node_id: u64,
    },
    DrainStatus,
    /// `keys {status|rotate|shred}`
    KeysStatus,
    KeysRotate,
    /// `keys shred <tenant-id> [--yes]` — IRREVERSIBLE crypto-shred.
    KeysShred {
        tenant_id: String,
        yes: bool,
    },
    /// `config show [--node N | --all]`
    ConfigShow {
        node: Option<String>,
        all: bool,
    },
}

fn print_usage() {
    eprintln!(
        "kiseki-admin -- remote cluster administration CLI\n\
         \n\
         Usage:\n\
         \x20 kiseki-admin [--endpoint URL] [--json] <command> [options]\n\
         \n\
         Commands:\n\
         \x20 status                         Cluster status summary\n\
         \x20 nodes                          Node list with health and metrics\n\
         \x20 events [--severity S] [--hours N]  Event log\n\
         \x20 history [--hours N]            Metric history time series\n\
         \x20 maintenance on|off             Toggle cluster maintenance mode\n\
         \x20 backup                         Trigger a backup\n\
         \x20 scrub                          Trigger an integrity scrub\n\
         \x20 shards                         Per-shard leader map (ADR-008 rev 2)\n\
         \x20 forwarding                     Proxy + stale-leader counters\n\
         \x20 tenant list [--type org|project|workload|namespace]\n\
         \x20 tenant create-org <name>\n\
         \x20 tenant create-project <org-id> <name>\n\
         \x20 tenant create-workload <project-id> <name>\n\
         \x20 tenant create-namespace <workload-id> <name>\n\
         \x20 tenant describe <id>\n\
         \x20 tenant delete <id> [--yes]\n\
         \x20 audit query [--tenant T] [--type X] [--limit N] [--from S] [--local-only]\n\
         \x20 snapshot create [--note T]     Create a snapshot\n\
         \x20 snapshot list                  List snapshots\n\
         \x20 snapshot restore <id>          Restore a snapshot\n\
         \x20 drain <node-id>                Initiate drain\n\
         \x20 drain status                   Show drain progress\n\
         \x20 drain cancel <node-id>         Cancel a drain\n\
         \x20 keys status                    Show key-manager epochs\n\
         \x20 keys rotate                    Rotate the system master key\n\
         \x20 keys shred <tenant-id> [--yes] Crypto-shred a tenant (IRREVERSIBLE)\n\
         \x20 config show [--node N | --all] Show runtime knobs\n\
         \x20 --version                      Print version\n\
         \x20 help                           Show this message\n\
         \n\
         Global flags:\n\
         \x20 --json                         Emit machine-readable JSON instead of human tables\n\
         \x20 --endpoint URL                 Defaults to KISEKI_ENDPOINT or http://localhost:9090"
    );
}

/// Parse the global `--endpoint` and `--json` flags. Returns (endpoint, json, remaining index).
fn parse_globals(args: &[String]) -> (String, bool, usize) {
    let mut endpoint: Option<String> = None;
    let mut json = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" => {
                i += 1;
                endpoint = args.get(i).cloned();
                i += 1;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            other if other.starts_with("--endpoint=") => {
                endpoint = Some(other.trim_start_matches("--endpoint=").to_string());
                i += 1;
            }
            _ => break,
        }
    }

    (endpoint.unwrap_or_else(default_endpoint), json, i)
}

/// Parse the subcommand and its options from remaining args.
fn parse_subcommand(args: &[String], start: usize) -> Result<Command, String> {
    if start >= args.len() {
        return Ok(Command::Help);
    }

    let cmd = args[start].as_str();
    let mut i = start + 1;

    match cmd {
        "status" => Ok(Command::Status),
        "nodes" => Ok(Command::Nodes),
        "events" => {
            let mut severity = None;
            let mut hours = None;
            while i < args.len() {
                match args[i].as_str() {
                    "--severity" => {
                        i += 1;
                        severity = Some(args.get(i).ok_or("--severity requires a value")?.clone());
                    }
                    "--hours" => {
                        i += 1;
                        hours = Some(
                            args.get(i)
                                .ok_or("--hours requires a value")?
                                .parse::<f64>()
                                .map_err(|_| "--hours must be a number")?,
                        );
                    }
                    other => return Err(format!("unknown option for events: {other}")),
                }
                i += 1;
            }
            Ok(Command::Events { severity, hours })
        }
        "history" => {
            let mut hours = None;
            while i < args.len() {
                match args[i].as_str() {
                    "--hours" => {
                        i += 1;
                        hours = Some(
                            args.get(i)
                                .ok_or("--hours requires a value")?
                                .parse::<f64>()
                                .map_err(|_| "--hours must be a number")?,
                        );
                    }
                    other => return Err(format!("unknown option for history: {other}")),
                }
                i += 1;
            }
            Ok(Command::History { hours })
        }
        "maintenance" => {
            let toggle = args
                .get(i)
                .ok_or("maintenance requires 'on' or 'off'")?
                .as_str();
            let enabled = match toggle {
                "on" => true,
                "off" => false,
                other => return Err(format!("maintenance expects 'on' or 'off', got '{other}'")),
            };
            Ok(Command::Maintenance { enabled })
        }
        "backup" => Ok(Command::Backup),
        "scrub" => Ok(Command::Scrub),
        "shards" => Ok(Command::Shards),
        "forwarding" => Ok(Command::Forwarding),
        "audit" => parse_audit(&args[i..]),
        "tenant" => parse_tenant(&args[i..]),
        "snapshot" => parse_snapshot(&args[i..]),
        "drain" => parse_drain(&args[i..]),
        "keys" => parse_keys(&args[i..]),
        "config" => parse_config(&args[i..]),
        "version" | "--version" | "-V" => Ok(Command::Version),
        "help" | "--help" | "-h" => Ok(Command::Help),
        other => Err(format!("unknown command: {other}")),
    }
}

fn parse_audit(rest: &[String]) -> Result<Command, String> {
    let sub = rest.first().ok_or("audit requires a subcommand (query)")?;
    if sub != "query" {
        return Err(format!("unknown audit subcommand: {sub}"));
    }
    let mut tenant = None;
    let mut event_type = None;
    let mut limit = None;
    let mut from = None;
    let mut local_only = false;
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--tenant" => {
                i += 1;
                tenant = Some(rest.get(i).ok_or("--tenant requires a value")?.clone());
            }
            "--type" => {
                i += 1;
                event_type = Some(rest.get(i).ok_or("--type requires a value")?.clone());
            }
            "--limit" => {
                i += 1;
                limit = Some(
                    rest.get(i)
                        .ok_or("--limit requires a value")?
                        .parse::<usize>()
                        .map_err(|_| "--limit must be a positive integer")?,
                );
            }
            "--from" => {
                i += 1;
                from = Some(
                    rest.get(i)
                        .ok_or("--from requires a value")?
                        .parse::<u64>()
                        .map_err(|_| "--from must be a positive integer")?,
                );
            }
            "--local-only" => {
                local_only = true;
            }
            other => return Err(format!("unknown audit query option: {other}")),
        }
        i += 1;
    }
    Ok(Command::AuditQuery {
        tenant,
        event_type,
        limit,
        from,
        local_only,
    })
}

fn parse_tenant(rest: &[String]) -> Result<Command, String> {
    let sub = rest
        .first()
        .ok_or("tenant requires a subcommand (list, create-org, create-project, create-workload, create-namespace, describe, delete)")?;
    match sub.as_str() {
        "list" => {
            // optional --type flag; default = orgs
            let mut kind = String::from("org");
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--type" => {
                        i += 1;
                        kind = rest.get(i).ok_or("--type requires a value")?.clone();
                    }
                    other => return Err(format!("unknown tenant list option: {other}")),
                }
                i += 1;
            }
            Ok(Command::TenantList { kind })
        }
        "create-org" => {
            let name = rest.get(1).ok_or("create-org requires <name>")?.clone();
            Ok(Command::TenantCreateOrg { name })
        }
        "create-project" => {
            let org_id = rest
                .get(1)
                .ok_or("create-project requires <org-id> <name>")?
                .clone();
            let name = rest
                .get(2)
                .ok_or("create-project requires <org-id> <name>")?
                .clone();
            Ok(Command::TenantCreateProject { org_id, name })
        }
        "create-workload" => {
            let project_id = rest
                .get(1)
                .ok_or("create-workload requires <project-id> <name>")?
                .clone();
            let name = rest
                .get(2)
                .ok_or("create-workload requires <project-id> <name>")?
                .clone();
            Ok(Command::TenantCreateWorkload { project_id, name })
        }
        "create-namespace" => {
            let workload_id = rest
                .get(1)
                .ok_or("create-namespace requires <workload-id> <name>")?
                .clone();
            let name = rest
                .get(2)
                .ok_or("create-namespace requires <workload-id> <name>")?
                .clone();
            Ok(Command::TenantCreateNamespace { workload_id, name })
        }
        "describe" => {
            let id = rest.get(1).ok_or("describe requires <id>")?.clone();
            Ok(Command::TenantDescribe { id })
        }
        "delete" => {
            let id = rest.get(1).ok_or("delete requires <id>")?.clone();
            let mut yes = false;
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--yes" => yes = true,
                    other => return Err(format!("unknown tenant delete option: {other}")),
                }
                i += 1;
            }
            Ok(Command::TenantDelete { id, yes })
        }
        other => Err(format!("unknown tenant subcommand: {other}")),
    }
}

fn parse_snapshot(rest: &[String]) -> Result<Command, String> {
    let sub = rest
        .first()
        .ok_or("snapshot requires a subcommand (create, list, restore)")?;
    match sub.as_str() {
        "create" => {
            let mut note = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--note" => {
                        i += 1;
                        note = Some(rest.get(i).ok_or("--note requires a value")?.clone());
                    }
                    other => return Err(format!("unknown snapshot create option: {other}")),
                }
                i += 1;
            }
            Ok(Command::SnapshotCreate { note })
        }
        "list" => Ok(Command::SnapshotList),
        "restore" => {
            let id = rest.get(1).ok_or("restore requires <snapshot-id>")?.clone();
            Ok(Command::SnapshotRestore { snapshot_id: id })
        }
        other => Err(format!("unknown snapshot subcommand: {other}")),
    }
}

fn parse_drain(rest: &[String]) -> Result<Command, String> {
    let head = rest
        .first()
        .ok_or("drain requires <node-id> or 'status' / 'cancel'")?;
    match head.as_str() {
        "status" => Ok(Command::DrainStatus),
        "cancel" => {
            let id = rest
                .get(1)
                .ok_or("drain cancel requires <node-id>")?
                .parse::<u64>()
                .map_err(|_| "node-id must be a positive integer")?;
            Ok(Command::DrainCancel { node_id: id })
        }
        other => {
            let id = other
                .parse::<u64>()
                .map_err(|_| format!("drain expects a node-id, got '{other}'"))?;
            Ok(Command::Drain { node_id: id })
        }
    }
}

fn parse_keys(rest: &[String]) -> Result<Command, String> {
    let sub = rest
        .first()
        .ok_or("keys requires a subcommand (status, rotate, shred)")?;
    match sub.as_str() {
        "status" => Ok(Command::KeysStatus),
        "rotate" => Ok(Command::KeysRotate),
        "shred" => {
            let tenant_id = rest
                .get(1)
                .ok_or("keys shred requires <tenant-id>")?
                .clone();
            let mut yes = false;
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--yes" => yes = true,
                    other => return Err(format!("unknown keys shred option: {other}")),
                }
                i += 1;
            }
            Ok(Command::KeysShred { tenant_id, yes })
        }
        other => Err(format!("unknown keys subcommand: {other}")),
    }
}

fn parse_config(rest: &[String]) -> Result<Command, String> {
    let sub = rest.first().ok_or("config requires 'show' subcommand")?;
    if sub != "show" {
        return Err(format!("unknown config subcommand: {sub}"));
    }
    let mut node = None;
    let mut all = false;
    let mut i = 1;
    while i < rest.len() {
        match rest[i].as_str() {
            "--node" => {
                i += 1;
                node = Some(rest.get(i).ok_or("--node requires a value")?.clone());
            }
            "--all" => {
                all = true;
                i += 1;
                continue;
            }
            other => return Err(format!("unknown config show option: {other}")),
        }
        i += 1;
    }
    Ok(Command::ConfigShow { node, all })
}

fn parse_args() -> Result<Args, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        return Ok(Args {
            endpoint: default_endpoint(),
            json: false,
            command: Command::Help,
        });
    }

    // Short-circuit --version before any other parsing so it works
    // even with no other args.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        return Ok(Args {
            endpoint: default_endpoint(),
            json: false,
            command: Command::Version,
        });
    }

    let (endpoint, json, sub_start) = parse_globals(&args);
    let command = parse_subcommand(&args, sub_start)?;

    Ok(Args {
        endpoint,
        json,
        command,
    })
}

fn default_endpoint() -> String {
    std::env::var("KISEKI_ENDPOINT").unwrap_or_else(|_| "http://localhost:9090".to_string())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{RED}error{RESET}: {e}");
            print_usage();
            std::process::exit(2);
        }
    };

    let json = args.json;
    let result = match args.command {
        Command::Status => http_get(&args.endpoint, "/ui/api/cluster").map(|b| {
            if json {
                b
            } else {
                format_cluster_status(&b)
            }
        }),
        Command::Nodes => {
            http_get(&args.endpoint, "/ui/api/nodes")
                .map(|b| if json { b } else { format_nodes(&b) })
        }
        Command::Events { severity, hours } => {
            let mut params = Vec::new();
            if let Some(s) = &severity {
                params.push(format!("severity={s}"));
            }
            if let Some(h) = hours {
                params.push(format!("hours={h}"));
            }
            let path = if params.is_empty() {
                "/ui/api/events".to_string()
            } else {
                format!("/ui/api/events?{}", params.join("&"))
            };
            http_get(&args.endpoint, &path).map(|b| if json { b } else { format_events(&b) })
        }
        Command::History { hours } => {
            let path = if let Some(h) = hours {
                format!("/ui/api/history?hours={h}")
            } else {
                "/ui/api/history".to_string()
            };
            http_get(&args.endpoint, &path).map(|b| if json { b } else { format_history(&b) })
        }
        Command::Maintenance { enabled } => {
            let body = format!(r#"{{"enabled":{enabled}}}"#);
            http_post(&args.endpoint, "/ui/api/ops/maintenance", &body).map(|b| {
                if json {
                    b
                } else {
                    format_ops_response(&b)
                }
            })
        }
        Command::Backup => http_post(&args.endpoint, "/ui/api/ops/backup", "{}").map(|b| {
            if json {
                b
            } else {
                format_ops_response(&b)
            }
        }),
        Command::Scrub => http_post(&args.endpoint, "/ui/api/ops/scrub", "{}").map(|b| {
            if json {
                b
            } else {
                format_ops_response(&b)
            }
        }),
        Command::Shards => http_get(&args.endpoint, "/admin/topology/shards").map(|b| {
            if json {
                b
            } else {
                format_shards(&b)
            }
        }),
        Command::Forwarding => http_get(&args.endpoint, "/admin/topology/forwarding").map(|b| {
            if json {
                b
            } else {
                format_forwarding(&b)
            }
        }),
        Command::AuditQuery {
            tenant,
            event_type,
            limit,
            from,
            local_only,
        } => {
            let mut params = Vec::new();
            if let Some(t) = &tenant {
                params.push(format!("tenant={t}"));
            }
            if let Some(t) = &event_type {
                params.push(format!("event_type={t}"));
            }
            if let Some(n) = limit {
                params.push(format!("limit={n}"));
            }
            if let Some(s) = from {
                params.push(format!("from={s}"));
            }
            if local_only {
                params.push("local_only=true".to_string());
            }
            let path = if params.is_empty() {
                "/admin/audit/query".to_string()
            } else {
                format!("/admin/audit/query?{}", params.join("&"))
            };
            http_get(&args.endpoint, &path).map(|b| if json { b } else { format_audit(&b) })
        }
        Command::TenantList { kind } => {
            let path = match kind.as_str() {
                "project" | "projects" => "/admin/tenants/projects",
                "workload" | "workloads" => "/admin/tenants/workloads",
                "namespace" | "namespaces" => "/admin/tenants/namespaces",
                _ => "/admin/tenants/orgs",
            };
            http_get(&args.endpoint, path).map(|b| if json { b } else { format_tenants(&b, &kind) })
        }
        Command::TenantCreateOrg { name } => {
            let body = format!(r#"{{"name":"{}"}}"#, json_escape(&name));
            http_post(&args.endpoint, "/admin/tenants/orgs", &body).map(|b| {
                if json {
                    b
                } else {
                    format!("Created org: {b}\n")
                }
            })
        }
        Command::TenantCreateProject { org_id, name } => {
            let body = format!(
                r#"{{"org_id":"{}","name":"{}"}}"#,
                json_escape(&org_id),
                json_escape(&name),
            );
            http_post(&args.endpoint, "/admin/tenants/projects", &body).map(|b| {
                if json {
                    b
                } else {
                    format!("Created project: {b}\n")
                }
            })
        }
        Command::TenantCreateWorkload { project_id, name } => {
            let body = format!(
                r#"{{"project_id":"{}","name":"{}"}}"#,
                json_escape(&project_id),
                json_escape(&name),
            );
            http_post(&args.endpoint, "/admin/tenants/workloads", &body).map(|b| {
                if json {
                    b
                } else {
                    format!("Created workload: {b}\n")
                }
            })
        }
        Command::TenantCreateNamespace { workload_id, name } => {
            let body = format!(
                r#"{{"workload_id":"{}","name":"{}"}}"#,
                json_escape(&workload_id),
                json_escape(&name),
            );
            http_post(&args.endpoint, "/admin/tenants/namespaces", &body).map(|b| {
                if json {
                    b
                } else {
                    format!("Created namespace: {b}\n")
                }
            })
        }
        Command::TenantDescribe { id } => {
            let path = format!("/admin/tenants/describe?id={}", url_encode(&id));
            http_get(&args.endpoint, &path).map(|b| if json { b } else { format!("{b}\n") })
        }
        Command::TenantDelete { id, yes } => {
            if !yes
                && !confirm_destructive(&format!("This permanently removes tenant `{id}`."), &id)
            {
                Ok(format!("{YELLOW}cancelled{RESET}\n"))
            } else {
                let body = format!(r#"{{"id":"{}"}}"#, json_escape(&id));
                http_post(&args.endpoint, "/admin/tenants/delete", &body).map(|b| {
                    if json {
                        b
                    } else {
                        format_ops_response(&b)
                    }
                })
            }
        }
        Command::SnapshotCreate { note } => {
            let body = note
                .map(|n| format!(r#"{{"note":"{}"}}"#, json_escape(&n)))
                .unwrap_or_else(|| "{}".to_string());
            http_post(&args.endpoint, "/admin/snapshots", &body).map(|b| {
                if json {
                    b
                } else {
                    format_ops_response(&b)
                }
            })
        }
        Command::SnapshotList => http_get(&args.endpoint, "/admin/snapshots").map(|b| {
            if json {
                b
            } else {
                format_snapshots(&b)
            }
        }),
        Command::SnapshotRestore { snapshot_id } => {
            let body = format!(r#"{{"snapshot_id":"{}"}}"#, json_escape(&snapshot_id));
            http_post(&args.endpoint, "/admin/snapshots/restore", &body).map(|b| {
                if json {
                    b
                } else {
                    format_ops_response(&b)
                }
            })
        }
        Command::Drain { node_id } => {
            let body = format!(r#"{{"node_id":{node_id}}}"#);
            http_post(&args.endpoint, "/admin/drains", &body).map(|b| {
                if json {
                    b
                } else {
                    format_ops_response(&b)
                }
            })
        }
        Command::DrainCancel { node_id } => {
            let body = format!(r#"{{"node_id":{node_id}}}"#);
            http_post(&args.endpoint, "/admin/drains/cancel", &body).map(|b| {
                if json {
                    b
                } else {
                    format_ops_response(&b)
                }
            })
        }
        Command::DrainStatus => {
            http_get(&args.endpoint, "/admin/drains")
                .map(|b| if json { b } else { format_drains(&b) })
        }
        Command::KeysStatus => http_get(&args.endpoint, "/admin/keys/status").map(|b| {
            if json {
                b
            } else {
                format_keys_status(&b)
            }
        }),
        Command::KeysRotate => http_post(&args.endpoint, "/admin/keys/rotate", "{}").map(|b| {
            if json {
                b
            } else {
                format_ops_response(&b)
            }
        }),
        Command::KeysShred { tenant_id, yes } => {
            if !yes
                && !confirm_destructive(
                    &format!(
                        "{RED}IRREVERSIBLE:{RESET} crypto-shred permanently destroys all data \
                         for tenant `{tenant_id}` (ADR-014 §K11)."
                    ),
                    &tenant_id,
                )
            {
                Ok(format!("{YELLOW}cancelled{RESET}\n"))
            } else {
                let body = format!(
                    r#"{{"tenant_id":"{}","reason":"kiseki-admin keys shred"}}"#,
                    json_escape(&tenant_id),
                );
                http_post(&args.endpoint, "/admin/keys/shred", &body).map(|b| {
                    if json {
                        b
                    } else {
                        format_ops_response(&b)
                    }
                })
            }
        }
        Command::ConfigShow { node, all } => {
            // The HTTP endpoint always reports the local node's config.
            // --all = scrape /admin/config on every peer (discovered via /cluster/info).
            // --node N picks one peer's metrics_addr from /cluster/info.
            if all {
                fetch_config_for_all_peers(&args.endpoint).map(|b| {
                    if json {
                        b
                    } else {
                        format_config_all(&b)
                    }
                })
            } else if let Some(n) = node {
                fetch_config_for_peer(&args.endpoint, &n).map(|b| {
                    if json {
                        b
                    } else {
                        format_config(&b)
                    }
                })
            } else {
                http_get(&args.endpoint, "/admin/config").map(|b| {
                    if json {
                        b
                    } else {
                        format_config(&b)
                    }
                })
            }
        }
        Command::Version => {
            println!("kiseki-admin {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Command::Help => {
            print_usage();
            std::process::exit(0);
        }
    };

    match result {
        Ok(output) => {
            print!("{output}");
        }
        Err(e) => {
            eprintln!("{RED}error{RESET}: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Formatters for new subcommand responses
// ---------------------------------------------------------------------------

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// URL-encode a query-string value. Only the limited subset needed by
/// the admin CLI (alphanumerics + `-._~` pass through; everything else
/// gets %-encoded). No external dependency.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Prompt the operator to retype the destructive-action target id.
/// Returns `true` only when stdin echoed the exact `target` string.
///
/// Skipped (returns `true`) when `--yes` was passed at the call site.
/// The caller is responsible for `--yes` short-circuit; this helper
/// only handles the interactive confirmation.
fn confirm_destructive(message: &str, target: &str) -> bool {
    eprintln!("{message}");
    eprint!("Type `{target}` to confirm (or anything else to cancel): ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    line.trim() == target
}

fn format_shards(body: &str) -> String {
    let arr = json_array_value(body, "shards").unwrap_or("[]");
    let shards = json_array_elements(arr);
    if shards.is_empty() {
        return "No shards reported.\n".to_string();
    }
    let mut out = format!(
        "\n{BOLD}{:<40} {:<24} {:<8} {:<24}{RESET}\n",
        "SHARD", "NAMESPACE", "LEADER", "LEADER-ADDR"
    );
    for s in &shards {
        let _ = writeln!(
            out,
            "{:<40} {:<24} {:<8} {:<24}",
            json_str(s, "shard_id").unwrap_or("?"),
            json_str(s, "namespace_id").unwrap_or("?"),
            json_u64(s, "leader_id")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            json_str(s, "leader_data_addr").unwrap_or("-"),
        );
    }
    out
}

fn format_forwarding(body: &str) -> String {
    let proxy_on = json_bool(body, "proxy_fallback_enabled").unwrap_or(false);
    let mut out = format!(
        "\n{BOLD}Proxy fallback:{RESET} {}\n",
        if proxy_on { "ON" } else { "off" }
    );
    let forwards = json_array_value(body, "forwards").unwrap_or("[]");
    let forwards = json_array_elements(forwards);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{:<14} {:<14} {:>10}{RESET}",
        "SOURCE", "LEADER", "FORWARDS"
    );
    if forwards.is_empty() {
        let _ = writeln!(out, "(none yet)");
    } else {
        for r in &forwards {
            // Each row has `{"labels":{"source_node":"1","leader_node":"2"},"value":3}`
            let labels = json_object_value(r, "labels").unwrap_or("{}");
            let val = json_u64(r, "value").unwrap_or(0);
            let _ = writeln!(
                out,
                "{:<14} {:<14} {:>10}",
                json_str(labels, "source_node").unwrap_or("?"),
                json_str(labels, "leader_node").unwrap_or("?"),
                val,
            );
        }
    }
    let stale = json_array_value(body, "stale_leader_redirects").unwrap_or("[]");
    let stale = json_array_elements(stale);
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{:<14} {:>10}{RESET}", "PROTOCOL", "REDIRECTS");
    if stale.is_empty() {
        let _ = writeln!(out, "(none yet)");
    } else {
        for r in &stale {
            let labels = json_object_value(r, "labels").unwrap_or("{}");
            let val = json_u64(r, "value").unwrap_or(0);
            let _ = writeln!(
                out,
                "{:<14} {:>10}",
                json_str(labels, "protocol").unwrap_or("?"),
                val,
            );
        }
    }
    out
}

fn format_audit(body: &str) -> String {
    let events_arr = json_array_value(body, "events").unwrap_or("[]");
    let events = json_array_elements(events_arr);
    if events.is_empty() {
        return "No audit events.\n".to_string();
    }
    let mut out = format!(
        "\n{BOLD}{:<8} {:<22} {:<20} {}{RESET}\n",
        "SEQ", "TYPE", "ACTOR", "DESCRIPTION"
    );
    for e in &events {
        let _ = writeln!(
            out,
            "{:<8} {:<22} {:<20} {}",
            json_u64(e, "sequence").unwrap_or(0),
            json_str(e, "type").unwrap_or("?"),
            json_str(e, "actor").unwrap_or("?"),
            json_str(e, "description").unwrap_or(""),
        );
    }
    out
}

fn format_tenants(body: &str, kind: &str) -> String {
    let key = match kind {
        "project" | "projects" => "projects",
        "workload" | "workloads" => "workloads",
        "namespace" | "namespaces" => "namespaces",
        _ => "orgs",
    };
    let arr = json_array_value(body, key).unwrap_or("[]");
    let items = json_array_elements(arr);
    if items.is_empty() {
        return format!("No {key}.\n");
    }
    let mut out = format!("\n{BOLD}{:<40} {}{RESET}\n", "ID", "NAME");
    for it in &items {
        let _ = writeln!(
            out,
            "{:<40} {}",
            json_str(it, "id").unwrap_or("?"),
            json_str(it, "name").unwrap_or(""),
        );
    }
    out
}

fn format_snapshots(body: &str) -> String {
    let arr = json_array_value(body, "snapshots").unwrap_or("[]");
    let snaps = json_array_elements(arr);
    if snaps.is_empty() {
        return "No snapshots.\n".to_string();
    }
    let mut out = format!(
        "\n{BOLD}{:<40} {:>12} {:>12} {}{RESET}\n",
        "SNAPSHOT-ID", "META-BYTES", "DATA-BYTES", "CREATED-AT"
    );
    for s in &snaps {
        let _ = writeln!(
            out,
            "{:<40} {:>12} {:>12} {}",
            json_str(s, "snapshot_id").unwrap_or("?"),
            json_u64(s, "metadata_bytes").unwrap_or(0),
            json_u64(s, "data_bytes").unwrap_or(0),
            json_str(s, "created_at").unwrap_or(""),
        );
    }
    out
}

fn format_drains(body: &str) -> String {
    let arr = json_array_value(body, "drains").unwrap_or("[]");
    let drains = json_array_elements(arr);
    if drains.is_empty() {
        return "No drains active.\n".to_string();
    }
    let mut out = format!(
        "\n{BOLD}{:<10} {:<12} {:>10}{RESET}\n",
        "NODE", "STATE", "SHARDS"
    );
    for d in &drains {
        let _ = writeln!(
            out,
            "{:<10} {:<12} {:>10}",
            json_u64(d, "node_id").unwrap_or(0),
            json_str(d, "state").unwrap_or("?"),
            json_u64(d, "voter_in_shards").unwrap_or(0),
        );
    }
    out
}

fn format_keys_status(body: &str) -> String {
    let cur = json_u64(body, "current_epoch")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    let mut out = format!("\n{BOLD}Current epoch:{RESET} {cur}\n");
    let epochs_arr = json_array_value(body, "epochs").unwrap_or("[]");
    let epochs = json_array_elements(epochs_arr);
    let _ = writeln!(
        out,
        "{BOLD}{:>6} {:<8} {}{RESET}",
        "EPOCH", "CURRENT", "MIGRATED"
    );
    for e in &epochs {
        let _ = writeln!(
            out,
            "{:>6} {:<8} {}",
            json_u64(e, "epoch").unwrap_or(0),
            json_bool(e, "is_current").unwrap_or(false),
            json_bool(e, "migration_complete").unwrap_or(false),
        );
    }
    out
}

fn format_config(body: &str) -> String {
    let node_id = json_u64(body, "node_id").unwrap_or(0);
    let mut out = format!("\n{BOLD}Node {node_id} config{RESET}\n");
    // The "config" object isn't easy to parse with the homegrown JSON
    // utilities (no recursive walk); just append the raw body for
    // operator readability.
    let cfg = json_object_value(body, "config").unwrap_or("{}");
    let _ = writeln!(out, "{cfg}");
    out
}

fn format_config_all(body: &str) -> String {
    // body is `{"nodes":[{node_id, config}, ...]}`
    let nodes_arr = json_array_value(body, "nodes").unwrap_or("[]");
    let nodes = json_array_elements(nodes_arr);
    let mut out = String::new();
    for n in &nodes {
        out.push_str(&format_config(n));
    }
    out
}

/// Discover all peer metrics endpoints from `/cluster/info` and scrape
/// `/admin/config` on each, returning a synthesized JSON object.
fn fetch_config_for_all_peers(endpoint: &str) -> Result<String, String> {
    let info = http_get(endpoint, "/cluster/info")?;
    let peers_arr = json_array_value(&info, "peers").unwrap_or("[]");
    let peers = json_array_elements(peers_arr);
    let mut out = String::from(r#"{"nodes":["#);
    for (i, p) in peers.iter().enumerate() {
        let metrics_addr = json_str(p, "metrics_addr").unwrap_or("");
        if metrics_addr.is_empty() {
            continue;
        }
        let url = format!("http://{metrics_addr}");
        let body = http_get(&url, "/admin/config").unwrap_or_else(|_| "{}".to_string());
        if i > 0 {
            out.push(',');
        }
        out.push_str(&body);
    }
    out.push_str("]}");
    Ok(out)
}

fn fetch_config_for_peer(endpoint: &str, node: &str) -> Result<String, String> {
    // `node` may be either a NodeId (decimal) or a `host:port`. If it
    // parses as integer, look it up via `/cluster/info` `peers`.
    if let Ok(target_id) = node.parse::<u64>() {
        let info = http_get(endpoint, "/cluster/info")?;
        let peers_arr = json_array_value(&info, "peers").unwrap_or("[]");
        let peers = json_array_elements(peers_arr);
        for p in &peers {
            if json_u64(p, "id") == Some(target_id) {
                let metrics_addr = json_str(p, "metrics_addr").unwrap_or("");
                if metrics_addr.is_empty() {
                    return Err(format!("node {target_id} has no metrics_addr"));
                }
                let url = format!("http://{metrics_addr}");
                return http_get(&url, "/admin/config");
            }
        }
        Err(format!("node {target_id} not found in /cluster/info peers"))
    } else {
        let url = if node.starts_with("http://") {
            node.to_string()
        } else {
            format!("http://{node}")
        };
        http_get(&url, "/admin/config")
    }
}

/// Extract a JSON object substring (`{...}`) for a key. Like
/// `json_array_value` but for `{...}` instead of `[...]`.
fn json_object_value<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    if !after_ws.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    for (i, c) in after_ws.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&after_ws[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests — argument parsers (no I/O).
//
// These cover the parser surface added under the 2026-05-15 UI/CLI
// follow-ups (D3 nested tenant CRUD, D4 keys shred, D5 audit
// --local-only). Each test isolates one option matrix so future
// drift surfaces a precise failure.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Command, String> {
        let owned: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        parse_subcommand(&owned, 0)
    }

    // --- D3: nested tenant CRUD ----------------------------------------

    #[test]
    fn tenant_create_project_requires_org_and_name() {
        let cmd = parse(&["tenant", "create-project", "ORG-1", "myproj"]).unwrap();
        match cmd {
            Command::TenantCreateProject { org_id, name } => {
                assert_eq!(org_id, "ORG-1");
                assert_eq!(name, "myproj");
            }
            _ => panic!("expected TenantCreateProject"),
        }
        assert!(parse(&["tenant", "create-project", "ORG-1"]).is_err());
        assert!(parse(&["tenant", "create-project"]).is_err());
    }

    #[test]
    fn tenant_create_workload_requires_project_and_name() {
        let cmd = parse(&["tenant", "create-workload", "PROJ-1", "trainer"]).unwrap();
        match cmd {
            Command::TenantCreateWorkload { project_id, name } => {
                assert_eq!(project_id, "PROJ-1");
                assert_eq!(name, "trainer");
            }
            _ => panic!("expected TenantCreateWorkload"),
        }
        assert!(parse(&["tenant", "create-workload"]).is_err());
    }

    #[test]
    fn tenant_create_namespace_requires_workload_and_name() {
        let cmd = parse(&["tenant", "create-namespace", "WL-1", "ns-a"]).unwrap();
        match cmd {
            Command::TenantCreateNamespace { workload_id, name } => {
                assert_eq!(workload_id, "WL-1");
                assert_eq!(name, "ns-a");
            }
            _ => panic!("expected TenantCreateNamespace"),
        }
    }

    #[test]
    fn tenant_describe_takes_one_id() {
        let cmd = parse(&["tenant", "describe", "ORG-1"]).unwrap();
        match cmd {
            Command::TenantDescribe { id } => assert_eq!(id, "ORG-1"),
            _ => panic!("expected TenantDescribe"),
        }
        assert!(parse(&["tenant", "describe"]).is_err());
    }

    #[test]
    fn tenant_delete_supports_yes_flag() {
        let cmd = parse(&["tenant", "delete", "ORG-1"]).unwrap();
        match cmd {
            Command::TenantDelete { id, yes } => {
                assert_eq!(id, "ORG-1");
                assert!(!yes);
            }
            _ => panic!("expected TenantDelete"),
        }
        let cmd_yes = parse(&["tenant", "delete", "ORG-1", "--yes"]).unwrap();
        match cmd_yes {
            Command::TenantDelete { id, yes } => {
                assert_eq!(id, "ORG-1");
                assert!(yes);
            }
            _ => panic!("expected TenantDelete with --yes"),
        }
        assert!(parse(&["tenant", "delete"]).is_err());
    }

    // --- D4: keys shred ------------------------------------------------

    #[test]
    fn keys_shred_requires_tenant_id() {
        let cmd = parse(&["keys", "shred", "TENANT-1"]).unwrap();
        match cmd {
            Command::KeysShred { tenant_id, yes } => {
                assert_eq!(tenant_id, "TENANT-1");
                assert!(!yes);
            }
            _ => panic!("expected KeysShred"),
        }
        let cmd_yes = parse(&["keys", "shred", "TENANT-1", "--yes"]).unwrap();
        match cmd_yes {
            Command::KeysShred { tenant_id, yes } => {
                assert_eq!(tenant_id, "TENANT-1");
                assert!(yes);
            }
            _ => panic!("expected KeysShred with --yes"),
        }
        assert!(parse(&["keys", "shred"]).is_err());
    }

    // --- D5: audit query supports --local-only -------------------------

    #[test]
    fn audit_query_defaults_to_cluster_aggregation() {
        let cmd = parse(&["audit", "query"]).unwrap();
        match cmd {
            Command::AuditQuery { local_only, .. } => assert!(!local_only),
            _ => panic!("expected AuditQuery"),
        }
    }

    #[test]
    fn audit_query_local_only_opts_out_of_aggregation() {
        let cmd = parse(&["audit", "query", "--local-only"]).unwrap();
        match cmd {
            Command::AuditQuery { local_only, .. } => assert!(local_only),
            _ => panic!("expected AuditQuery"),
        }
    }

    #[test]
    fn audit_query_local_only_works_with_filters() {
        let cmd = parse(&[
            "audit",
            "query",
            "--tenant",
            "00000000-0000-0000-0000-000000000001",
            "--limit",
            "10",
            "--local-only",
        ])
        .unwrap();
        match cmd {
            Command::AuditQuery {
                tenant,
                limit,
                local_only,
                ..
            } => {
                assert_eq!(
                    tenant.as_deref(),
                    Some("00000000-0000-0000-0000-000000000001")
                );
                assert_eq!(limit, Some(10));
                assert!(local_only);
            }
            _ => panic!("expected AuditQuery"),
        }
    }
}
