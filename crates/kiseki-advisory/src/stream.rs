//! TCP-based advisory stream handler for non-gRPC clients.
//!
//! Accepts persistent connections from clients, receives JSON-encoded hints,
//! and sends JSON-encoded telemetry/ack responses back. Wire format is
//! length-prefixed JSON: 4-byte big-endian length prefix followed by a
//! UTF-8 JSON payload.
//!
//! This complements the gRPC `AdvisoryStream` bidi RPC for clients that
//! do not link tonic (e.g. `kiseki-client` native library). The advisory
//! service listens on a separate TCP port (default 9102).
//!
//! # Protocol
//!
//! Client sends:
//! ```json
//! {"type":"access_pattern","pattern":"sequential","file_id":42}
//! {"type":"prefetch","file_id":1,"offset":0,"length":4096}
//! {"type":"phase_advance","workflow_id":"...","phase":"training"}
//! {"type":"profile","profile":"ai_training"}
//! {"type":"heartbeat"}
//! ```
//!
//! Server responds per hint:
//! ```json
//! {"type":"ack","accepted":true}
//! {"type":"ack","accepted":false,"reason":"budget_exceeded"}
//! ```

use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::budget::BudgetEnforcer;
use kiseki_common::locks::LockOrWarn;

/// Maximum message size (64 KiB, per I-WA16).
const MAX_MSG_SIZE: usize = 65_536;

/// Run the TCP advisory stream server.
///
/// Listens on `addr` and spawns a task per client connection.
/// Each connection is independent and processes hints sequentially.
///
/// # Errors
///
/// Returns an I/O error if the listener cannot bind to `addr`.
pub async fn run_advisory_stream_server(
    addr: std::net::SocketAddr,
    budget: std::sync::Arc<Mutex<BudgetEnforcer>>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "advisory TCP stream server listening");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let budget = std::sync::Arc::clone(&budget);
                tokio::spawn(async move {
                    handle_advisory_connection(stream, budget, peer).await;
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "advisory TCP accept error");
            }
        }
    }
}

async fn handle_advisory_connection(
    mut stream: tokio::net::TcpStream,
    budget: std::sync::Arc<Mutex<BudgetEnforcer>>,
    peer: std::net::SocketAddr,
) {
    tracing::debug!(%peer, "advisory TCP client connected");

    loop {
        // Read 4-byte length prefix.
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len > MAX_MSG_SIZE {
            tracing::debug!(%peer, msg_len, "advisory TCP message too large, closing");
            break;
        }
        if msg_len == 0 {
            continue;
        }

        // Read message body.
        let mut msg_buf = vec![0u8; msg_len];
        if stream.read_exact(&mut msg_buf).await.is_err() {
            break;
        }

        // Parse and process.
        let (accepted, reason) = if let Ok(json_str) = std::str::from_utf8(&msg_buf) {
            tracing::debug!(%peer, hint = json_str, "advisory TCP hint received");
            process_hint_json(json_str, &budget)
        } else {
            tracing::debug!(%peer, "advisory TCP invalid UTF-8");
            (false, Some("invalid_utf8".to_owned()))
        };

        // Send ack response.
        let ack = if let Some(reason) = reason {
            format!(r#"{{"type":"ack","accepted":false,"reason":"{reason}"}}"#)
        } else {
            r#"{"type":"ack","accepted":true}"#.to_owned()
        };
        let ack_bytes = ack.as_bytes();
        // Ack JSON is always small (well under u32::MAX).
        #[allow(clippy::cast_possible_truncation)]
        let len = (ack_bytes.len() as u32).to_be_bytes();
        if stream.write_all(&len).await.is_err()
            || stream.write_all(ack_bytes).await.is_err()
            || stream.flush().await.is_err()
        {
            break;
        }

        if !accepted {
            tracing::trace!(%peer, "hint not accepted (non-fatal)");
        }
    }

    tracing::debug!(%peer, "advisory TCP client disconnected");
}

fn process_hint_json(
    json_str: &str,
    budget: &std::sync::Arc<Mutex<BudgetEnforcer>>,
) -> (bool, Option<String>) {
    // Parse just the "type" field to decide action. We use serde_json::Value
    // to avoid coupling to the client's AdvisoryHint enum.
    let val: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (false, Some("invalid_json".to_owned())),
    };

    let hint_type = val
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Heartbeats are always accepted, no budget check.
    if hint_type == "heartbeat" {
        return (true, None);
    }

    // Budget check.
    let mut b = budget.lock().lock_or_warn("stream.budget");
    match b.try_hint() {
        Ok(()) => (true, None),
        Err(e) => (false, Some(format!("budget_exceeded: {e}"))),
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::budget::BudgetConfig;
    use std::sync::Arc;

    fn test_budget() -> Arc<Mutex<BudgetEnforcer>> {
        Arc::new(Mutex::new(BudgetEnforcer::new(BudgetConfig {
            hints_per_sec: 5,
            max_concurrent_workflows: 2,
            max_phases_per_workflow: 10,
        })))
    }

    #[test]
    fn process_valid_hint() {
        let budget = test_budget();
        let (accepted, reason) = process_hint_json(
            r#"{"type":"access_pattern","pattern":"sequential","file_id":1}"#,
            &budget,
        );
        assert!(accepted);
        assert!(reason.is_none());
    }

    #[test]
    fn process_heartbeat_no_budget() {
        let budget = test_budget();
        // Exhaust budget first.
        {
            let mut b = budget.lock().unwrap();
            for _ in 0..5 {
                b.try_hint().unwrap();
            }
        }
        // Heartbeat should still be accepted.
        let (accepted, reason) = process_hint_json(r#"{"type":"heartbeat"}"#, &budget);
        assert!(accepted);
        assert!(reason.is_none());
    }

    #[test]
    fn process_invalid_json() {
        let budget = test_budget();
        let (accepted, reason) = process_hint_json("not json at all", &budget);
        assert!(!accepted);
        assert_eq!(reason.as_deref(), Some("invalid_json"));
    }

    #[test]
    fn process_hint_budget_exceeded() {
        let budget = test_budget();
        // Exhaust budget.
        {
            let mut b = budget.lock().unwrap();
            for _ in 0..5 {
                b.try_hint().unwrap();
            }
        }
        let (accepted, reason) = process_hint_json(
            r#"{"type":"prefetch","file_id":1,"offset":0,"length":100}"#,
            &budget,
        );
        assert!(!accepted);
        assert!(reason.unwrap().contains("budget_exceeded"));
    }

    #[test]
    fn hint_rejection_returns_unchanged_result() {
        // Verify that a rejected hint (budget exceeded) does not alter any
        // data-path state. Advisory is isolated from the data path by design
        // (I-WA2), so the only observable side-effect is the budget counter
        // itself — no external state should change.
        let budget = Arc::new(Mutex::new(BudgetEnforcer::new(BudgetConfig {
            hints_per_sec: 1,
            max_concurrent_workflows: 2,
            max_phases_per_workflow: 10,
        })));

        // Consume the single allowed hint.
        {
            let mut b = budget.lock().unwrap();
            b.try_hint().unwrap();
        }

        // Snapshot budget state before rejection.
        let (hints_before, workflows_before) = {
            let b = budget.lock().unwrap();
            (b.hints_used(), b.active_workflows())
        };

        // Submit a hint that will be rejected.
        let (accepted, reason) = process_hint_json(
            r#"{"type":"access_pattern","pattern":"random","file_id":99}"#,
            &budget,
        );
        assert!(!accepted, "hint should be rejected when budget exhausted");
        assert!(
            reason.as_ref().unwrap().contains("budget_exceeded"),
            "rejection reason should indicate budget_exceeded"
        );

        // Verify budget state is unchanged after rejection — the rejected
        // hint must not consume a token or alter workflow count.
        let (hints_after, workflows_after) = {
            let b = budget.lock().unwrap();
            (b.hints_used(), b.active_workflows())
        };
        assert_eq!(
            hints_before, hints_after,
            "hint counter must not change on rejection"
        );
        assert_eq!(
            workflows_before, workflows_after,
            "workflow count must not change on rejection"
        );
    }

    #[tokio::test]
    async fn server_accepts_connection_and_processes_hint() {
        let budget = test_budget();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Start server in background.
        let server_budget = Arc::clone(&budget);
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_advisory_connection(stream, server_budget, peer).await;
        });

        // Connect client.
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Send a hint.
        let hint = br#"{"type":"access_pattern","pattern":"sequential","file_id":42}"#;
        let len = (hint.len() as u32).to_be_bytes();
        client.write_all(&len).await.unwrap();
        client.write_all(hint).await.unwrap();
        client.flush().await.unwrap();

        // Read ack.
        let mut ack_len = [0u8; 4];
        client.read_exact(&mut ack_len).await.unwrap();
        let ack_size = u32::from_be_bytes(ack_len) as usize;
        let mut ack_buf = vec![0u8; ack_size];
        client.read_exact(&mut ack_buf).await.unwrap();

        let ack: serde_json::Value = serde_json::from_slice(&ack_buf).unwrap();
        assert_eq!(ack["type"], "ack");
        assert_eq!(ack["accepted"], true);
    }

    #[tokio::test]
    async fn client_handles_budget_exceeded() {
        let budget = Arc::new(Mutex::new(BudgetEnforcer::new(BudgetConfig {
            hints_per_sec: 1,
            max_concurrent_workflows: 1,
            max_phases_per_workflow: 1,
        })));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_budget = Arc::clone(&budget);
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_advisory_connection(stream, server_budget, peer).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Send two hints — second should be throttled.
        for i in 0..2 {
            let hint = format!(r#"{{"type":"prefetch","file_id":{i},"offset":0,"length":100}}"#);
            let hint_bytes = hint.as_bytes();
            let len = (hint_bytes.len() as u32).to_be_bytes();
            client.write_all(&len).await.unwrap();
            client.write_all(hint_bytes).await.unwrap();
            client.flush().await.unwrap();

            let mut ack_len = [0u8; 4];
            client.read_exact(&mut ack_len).await.unwrap();
            let ack_size = u32::from_be_bytes(ack_len) as usize;
            let mut ack_buf = vec![0u8; ack_size];
            client.read_exact(&mut ack_buf).await.unwrap();

            let ack: serde_json::Value = serde_json::from_slice(&ack_buf).unwrap();
            if i == 0 {
                assert_eq!(ack["accepted"], true);
            } else {
                assert_eq!(ack["accepted"], false);
            }
        }
    }
}
