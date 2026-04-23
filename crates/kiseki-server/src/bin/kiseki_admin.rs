//! kiseki-admin — remote cluster administration CLI.
//!
//! Connects to any Kiseki node via the REST API at `:9090`.
//!
//! Default endpoint: `localhost:9090` (or `KISEKI_ENDPOINT` env var).
#![allow(
    clippy::doc_markdown,
    clippy::manual_strip,
    clippy::missing_docs_in_private_items,
    clippy::format_push_string
)]

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

    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;

    let text = String::from_utf8_lossy(&buf);
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed HTTP response")?;

    // Check for chunked transfer-encoding and decode if needed.
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

    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;

    let text = String::from_utf8_lossy(&buf);
    let body_start = text
        .find("\r\n\r\n")
        .map(|i| i + 4)
        .ok_or("malformed HTTP response")?;

    let resp_body = &text[body_start..];
    if text[..body_start]
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        Ok(decode_chunked(resp_body))
    } else {
        Ok(resp_body.to_string())
    }
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
            // skip trailing \r\n after chunk data
            if remaining.starts_with("\r\n") {
                remaining = &remaining[2..];
            }
        } else {
            // Incomplete chunk — take what we have.
            result.push_str(&remaining_trimmed[data_start..]);
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Minimal JSON helpers (no serde — this binary has no deps beyond std)
// ---------------------------------------------------------------------------

/// Extract a string value for a given key from a flat JSON object.
fn json_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    // skip `:` and whitespace
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    if after_ws.starts_with('"') {
        let start = 1;
        let end = after_ws[1..].find('"')? + 1;
        Some(&after_ws[start..end])
    } else {
        None
    }
}

/// Extract a numeric value (u64) for a given key from a flat JSON object.
fn json_u64(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{key}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    // Read digits (and dots, for floats we'll truncate).
    let end = after_ws
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(after_ws.len());
    let num_str = &after_ws[..end];
    // Handle float by truncating.
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

/// Split a JSON array (top-level `[...]`) into individual object strings.
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
    // Find matching bracket.
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

fn format_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
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
    // Format with thousands separators.
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

fn format_cluster_status(body: &str) -> String {
    let total = json_u64(body, "total_nodes").unwrap_or(0);
    let healthy = json_u64(body, "healthy_nodes").unwrap_or(0);

    // The aggregate object is nested — find it.
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

        // Summary is nested.
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

        out.push_str(&format!(
            "{:<18}{color}{:<10}{RESET}{:<10}{:<10}{:<10}{:<10}{:<6}\n",
            addr,
            status,
            format_number(raft),
            format_number(requests),
            format_bytes(written),
            format_bytes(read),
            conns,
        ));
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

        // Show just HH:MM:SS from the timestamp if it contains 'T'.
        let time_short = if let Some(t_pos) = time.find('T') {
            let after_t = &time[t_pos + 1..];
            // Take up to 8 chars (HH:MM:SS).
            &after_t[..after_t.len().min(8)]
        } else if time.len() > 8 {
            &time[..8]
        } else {
            time
        };

        out.push_str(&format!(
            "{:<10}{color}{:<10}{RESET}{:<12}{:<12}{}\n",
            time_short,
            severity.to_ascii_uppercase(),
            category,
            source,
            message,
        ));
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

        let time_short = if let Some(t_pos) = time.find('T') {
            let after_t = &time[t_pos + 1..];
            &after_t[..after_t.len().min(8)]
        } else {
            time
        };

        out.push_str(&format!(
            "{:<12}{:<10}{:<10}{:<10}{:<10}{:<10}{:<6}\n",
            time_short,
            format_number(raft),
            format_number(requests),
            format_bytes(written),
            format_bytes(read),
            conns,
            format_number(deltas),
        ));
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
}

fn print_usage() {
    eprintln!(
        "kiseki-admin — remote cluster administration CLI\n\
         \n\
         Usage:\n\
         \x20 kiseki-admin [--endpoint URL] <command> [options]\n\
         \n\
         Commands:\n\
         \x20 status                         Cluster status summary\n\
         \x20 nodes                          Node list with health and metrics\n\
         \x20 events [--severity S] [--hours N]  Event log (severity: info|warning|error|critical)\n\
         \x20 history [--hours N]            Metric history time series\n\
         \x20 maintenance on|off             Toggle cluster maintenance mode\n\
         \x20 backup                         Trigger a backup\n\
         \x20 scrub                          Trigger an integrity scrub\n\
         \x20 help                           Show this message\n\
         \n\
         Endpoint defaults to KISEKI_ENDPOINT env var, or http://localhost:9090"
    );
}

fn parse_args() -> Result<Args, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        return Ok(Args {
            endpoint: default_endpoint(),
            command: Command::Help,
        });
    }

    let mut endpoint: Option<String> = None;
    let mut i = 0;

    // Parse --endpoint before subcommand.
    while i < args.len() {
        if args[i] == "--endpoint" {
            i += 1;
            endpoint = Some(args.get(i).ok_or("--endpoint requires a value")?.clone());
            i += 1;
        } else if args[i].starts_with("--endpoint=") {
            endpoint = Some(args[i]["--endpoint=".len()..].to_string());
            i += 1;
        } else {
            break;
        }
    }

    let endpoint = endpoint.unwrap_or_else(default_endpoint);

    if i >= args.len() {
        return Ok(Args {
            endpoint,
            command: Command::Help,
        });
    }

    let cmd = args[i].as_str();
    i += 1;

    let command = match cmd {
        "status" => Command::Status,
        "nodes" => Command::Nodes,
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
            Command::Events { severity, hours }
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
            Command::History { hours }
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
            Command::Maintenance { enabled }
        }
        "backup" => Command::Backup,
        "scrub" => Command::Scrub,
        "help" | "--help" | "-h" => Command::Help,
        other => return Err(format!("unknown command: {other}")),
    };

    Ok(Args { endpoint, command })
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

    let result =
        match args.command {
            Command::Status => {
                http_get(&args.endpoint, "/ui/api/cluster").map(|b| format_cluster_status(&b))
            }
            Command::Nodes => http_get(&args.endpoint, "/ui/api/nodes").map(|b| format_nodes(&b)),
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
                http_get(&args.endpoint, &path).map(|b| format_events(&b))
            }
            Command::History { hours } => {
                let path = if let Some(h) = hours {
                    format!("/ui/api/history?hours={h}")
                } else {
                    "/ui/api/history".to_string()
                };
                http_get(&args.endpoint, &path).map(|b| format_history(&b))
            }
            Command::Maintenance { enabled } => {
                let body = format!(r#"{{"enabled":{enabled}}}"#);
                http_post(&args.endpoint, "/ui/api/ops/maintenance", &body)
                    .map(|b| format_ops_response(&b))
            }
            Command::Backup => http_post(&args.endpoint, "/ui/api/ops/backup", "{}")
                .map(|b| format_ops_response(&b)),
            Command::Scrub => http_post(&args.endpoint, "/ui/api/ops/scrub", "{}")
                .map(|b| format_ops_response(&b)),
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
