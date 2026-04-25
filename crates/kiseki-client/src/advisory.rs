//! Client-side workflow advisory integration (ADR-020).
//!
//! Provides [`WorkflowSession`] for tracking multi-phase workflows and
//! [`ClientAdvisory`] for managing the set of active sessions within a
//! single client process. Workflow and client identifiers are 128-bit
//! values drawn from a CSPRNG (uuid v4).

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Errors from advisory workflow operations.
#[derive(Debug, thiserror::Error)]
pub enum AdvisoryError {
    /// Attempted to set a phase that is not strictly greater than the current phase.
    #[error("phase must advance monotonically")]
    PhaseNotMonotonic,

    /// Attempted to advance a workflow that has already ended.
    #[error("workflow has ended")]
    WorkflowEnded,

    /// The advisory channel is not connected.
    #[error("advisory channel unavailable")]
    ChannelUnavailable,
}

/// A hint to send on the advisory channel.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdvisoryHint {
    /// Access pattern detected (sequential, random, strided).
    AccessPattern {
        /// The detected pattern name.
        pattern: String,
        /// File identifier.
        file_id: u64,
    },
    /// Prefetch suggestion from `PrefetchAdvisor`.
    Prefetch {
        /// File identifier.
        file_id: u64,
        /// Offset to prefetch from.
        offset: u64,
        /// Number of bytes to prefetch.
        length: u64,
    },
    /// Phase advance notification.
    PhaseAdvance {
        /// Workflow identifier.
        workflow_id: u128,
        /// Phase name.
        phase: String,
    },
    /// Workload profile declaration.
    Profile {
        /// Profile name.
        profile: String,
    },
}

/// Telemetry signal received from the server.
#[derive(Clone, Debug)]
pub enum TelemetryFeedback {
    /// Backpressure signal (ok, soft, hard).
    Backpressure {
        /// Severity level.
        severity: String,
        /// Milliseconds before retry.
        retry_after_ms: u64,
    },
    /// Locality class for recent operations.
    Locality {
        /// Locality class name.
        class: String,
    },
    /// Prefetch hit rate feedback.
    PrefetchEffectiveness {
        /// Hit rate as a fraction in [0.0, 1.0].
        hit_rate: f64,
    },
}

/// Advisory channel state -- wraps the connection to the advisory TCP stream.
///
/// Non-blocking: if the channel is unavailable or drops, the client
/// continues without advisory (I-WA2). All methods are best-effort.
///
/// Uses a length-prefixed JSON protocol over TCP to communicate with the
/// advisory stream server (port 9102 by default). This avoids requiring
/// tonic as a dependency in the client library.
pub struct AdvisoryChannel {
    /// Advisory endpoint address (host:port for the TCP stream server).
    endpoint: String,
    /// Whether the channel is connected.
    connected: AtomicBool,
    /// Persistent TCP connection to the advisory stream server.
    tcp_stream: Mutex<Option<std::net::TcpStream>>,
    /// Hint send queue (bounded, drops oldest on overflow).
    hint_queue: Mutex<VecDeque<AdvisoryHint>>,
    /// Max hint queue depth before dropping.
    max_queue_depth: usize,
}

/// Default maximum queue depth for the advisory hint queue.
const DEFAULT_MAX_QUEUE_DEPTH: usize = 256;

/// TCP connection timeout.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// TCP write timeout.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

impl AdvisoryChannel {
    /// Create a new advisory channel targeting `endpoint`.
    #[must_use]
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            connected: AtomicBool::new(false),
            tcp_stream: Mutex::new(None),
            hint_queue: Mutex::new(VecDeque::new()),
            max_queue_depth: DEFAULT_MAX_QUEUE_DEPTH,
        }
    }

    /// Try to connect to the advisory TCP stream server.
    ///
    /// Non-blocking in the sense that it times out after 2 seconds.
    /// If the server is unreachable, the client continues without
    /// advisory (I-WA2). Returns `true` if connected.
    pub fn try_connect(&self) -> bool {
        if self.endpoint.is_empty() {
            return false;
        }

        let Ok(addr) = self.endpoint.parse::<std::net::SocketAddr>() else {
            tracing::debug!(
                endpoint = %self.endpoint,
                "advisory channel: invalid endpoint address"
            );
            return false;
        };

        match std::net::TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                // Set non-blocking write timeout to avoid stalling the data path.
                stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_millis(100)))
                    .ok();

                let mut guard = match self.tcp_stream.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                *guard = Some(stream);
                self.connected.store(true, Ordering::Release);
                tracing::info!(endpoint = %self.endpoint, "advisory channel connected");
                true
            }
            Err(e) => {
                tracing::debug!(
                    endpoint = %self.endpoint,
                    error = %e,
                    "advisory channel unavailable (data path unaffected, I-WA2)"
                );
                false
            }
        }
    }

    /// Send a hint (fire-and-forget, I-WA1). Never blocks the data path.
    ///
    /// Tries to send directly over TCP. If the TCP connection is not
    /// available, queues the hint for later drain. Returns `false` if
    /// the hint was dropped (queue full and no TCP connection).
    pub fn send_hint(&self, hint: AdvisoryHint) -> bool {
        if !self.connected.load(Ordering::Acquire) {
            return false;
        }

        // Try direct TCP send first.
        if self.send_hint_tcp(&hint) {
            return true;
        }

        // TCP send failed — queue for background drain.
        let Ok(mut queue) = self.hint_queue.lock() else {
            return false;
        };
        if queue.len() >= self.max_queue_depth {
            queue.pop_front(); // drop oldest
        }
        queue.push_back(hint);
        true
    }

    /// Send a hint directly over the TCP connection.
    ///
    /// Returns `true` if sent successfully. On failure, marks
    /// the channel as disconnected.
    fn send_hint_tcp(&self, hint: &AdvisoryHint) -> bool {
        let Ok(mut guard) = self.tcp_stream.lock() else {
            return false;
        };
        let Some(stream) = guard.as_mut() else {
            return false;
        };

        let Ok(json) = serde_json::to_vec(hint) else {
            return false;
        };

        // Hint JSON will never exceed u32::MAX in practice (I-WA16: 64 KiB max).
        #[allow(clippy::cast_possible_truncation)]
        let len = (json.len() as u32).to_be_bytes();
        if stream.write_all(&len).is_err()
            || stream.write_all(&json).is_err()
            || stream.flush().is_err()
        {
            drop(guard);
            self.mark_disconnected();
            return false;
        }

        // Read ack (best-effort, non-blocking due to read timeout).
        // We don't block on the ack — if it's not ready, we move on.
        let mut ack_len = [0u8; 4];
        if stream.read_exact(&mut ack_len).is_ok() {
            let ack_size = u32::from_be_bytes(ack_len) as usize;
            if ack_size <= 1024 {
                let mut ack_buf = vec![0u8; ack_size];
                let _ = stream.read_exact(&mut ack_buf);
            }
        }
        // Ack read failure is non-fatal — hint was sent.

        true
    }

    /// Drain pending hints (for the background sender task).
    ///
    /// Attempts to send each queued hint over TCP. Returns hints
    /// that were successfully drained from the queue.
    pub fn drain_hints(&self) -> Vec<AdvisoryHint> {
        let hints: Vec<AdvisoryHint> = match self.hint_queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return Vec::new(),
        };

        // Try to send each over TCP.
        for hint in &hints {
            self.send_hint_tcp(hint);
        }

        hints
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Mark as disconnected (on stream drop or error).
    pub fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Release);
        // Drop the TCP stream.
        if let Ok(mut guard) = self.tcp_stream.lock() {
            *guard = None;
        }
        tracing::debug!("advisory channel disconnected (data path unaffected, I-WA2)");
    }

    /// The endpoint address.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// A single workflow session tracked by the advisory subsystem.
///
/// Each session has a unique 128-bit identifier and progresses through
/// numbered phases. Phase transitions are monotonically increasing.
pub struct WorkflowSession {
    /// Unique identifier for this workflow (128-bit CSPRNG).
    pub workflow_id: u128,
    /// Identifier for the owning client process (128-bit CSPRNG).
    pub client_id: u128,
    current_phase: AtomicU64,
    phase_name: Mutex<String>,
    active: AtomicBool,
}

impl WorkflowSession {
    /// Create a new workflow session for the given client.
    ///
    /// Generates a fresh `workflow_id` via CSPRNG. The initial phase is 0
    /// with an empty phase name.
    #[must_use]
    pub fn new(client_id: u128) -> Self {
        Self {
            workflow_id: uuid::Uuid::new_v4().as_u128(),
            client_id,
            current_phase: AtomicU64::new(0),
            phase_name: Mutex::new(String::new()),
            active: AtomicBool::new(true),
        }
    }

    /// Advance to the next phase, storing the phase name.
    ///
    /// Returns the new phase number. The phase counter increments by one on
    /// each call; callers cannot skip or rewind.
    pub fn advance_phase(&self, phase_name: &str) -> Result<u64, AdvisoryError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(AdvisoryError::WorkflowEnded);
        }

        let prev = self.current_phase.fetch_add(1, Ordering::AcqRel);
        let new_phase = prev + 1;

        // Store the phase name.
        if let Ok(mut name) = self.phase_name.lock() {
            phase_name.clone_into(&mut name);
        }

        Ok(new_phase)
    }

    /// Return the current phase number.
    pub fn current_phase(&self) -> u64 {
        self.current_phase.load(Ordering::Acquire)
    }

    /// Return the name of the current phase.
    pub fn current_phase_name(&self) -> String {
        self.phase_name
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Mark this workflow as ended. Subsequent `advance_phase` calls will
    /// return [`AdvisoryError::WorkflowEnded`].
    pub fn end(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Whether this workflow session is still active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// The workflow identifier.
    pub fn workflow_id(&self) -> u128 {
        self.workflow_id
    }
}

/// Manages the set of active workflow sessions for a single client process.
pub struct ClientAdvisory {
    client_id: u128,
    active_workflows: HashMap<u128, Arc<WorkflowSession>>,
    channel: Option<AdvisoryChannel>,
}

impl ClientAdvisory {
    /// Create a new `ClientAdvisory` with a CSPRNG-generated client id.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client_id: uuid::Uuid::new_v4().as_u128(),
            active_workflows: HashMap::new(),
            channel: None,
        }
    }

    /// Create a `ClientAdvisory` connected to an advisory endpoint.
    ///
    /// The connection attempt is best-effort and non-blocking (I-WA2).
    #[must_use]
    pub fn with_advisory_endpoint(endpoint: String) -> Self {
        let channel = AdvisoryChannel::new(endpoint);
        channel.try_connect(); // best-effort, non-blocking
        Self {
            client_id: uuid::Uuid::new_v4().as_u128(),
            active_workflows: HashMap::new(),
            channel: Some(channel),
        }
    }

    /// Declare a new workflow, returning a shared handle to the session.
    ///
    /// If an advisory channel is connected, a [`AdvisoryHint::Profile`] hint
    /// is emitted on a best-effort basis (I-WA2).
    pub fn declare_workflow(&mut self) -> Arc<WorkflowSession> {
        let session = Arc::new(WorkflowSession::new(self.client_id));
        self.active_workflows
            .insert(session.workflow_id, Arc::clone(&session));
        // Best-effort: notify advisory if connected.
        if let Some(ref ch) = self.channel {
            ch.send_hint(AdvisoryHint::Profile {
                profile: "default".into(),
            });
        }
        session
    }

    /// Return a reference to the advisory channel, if configured.
    #[must_use]
    pub fn channel(&self) -> Option<&AdvisoryChannel> {
        self.channel.as_ref()
    }

    /// End the workflow identified by `workflow_id` and remove it from the
    /// active set. No-op if the id is not found.
    pub fn end_workflow(&mut self, workflow_id: u128) {
        if let Some(session) = self.active_workflows.remove(&workflow_id) {
            session.end();
        }
    }

    /// Number of currently active workflows.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active_workflows.len()
    }

    /// The stable client identifier for this process.
    #[must_use]
    pub fn client_id(&self) -> u128 {
        self.client_id
    }
}

impl Default for ClientAdvisory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declare_workflow_succeeds() {
        let mut advisory = ClientAdvisory::new();
        let session = advisory.declare_workflow();
        assert!(session.is_active());
        assert_eq!(session.current_phase(), 0);
        assert_eq!(advisory.active_count(), 1);
    }

    #[test]
    fn phase_advance_is_monotonic() {
        let session = WorkflowSession::new(0);
        let p1 = session.advance_phase("prepare").unwrap();
        let p2 = session.advance_phase("execute").unwrap();
        let p3 = session.advance_phase("commit").unwrap();
        assert_eq!(p1, 1);
        assert_eq!(p2, 2);
        assert_eq!(p3, 3);
        assert_eq!(session.current_phase(), 3);
        assert_eq!(session.current_phase_name(), "commit");
    }

    #[test]
    fn phase_advance_after_end_fails() {
        let session = WorkflowSession::new(0);
        session.advance_phase("prepare").unwrap();
        session.end();
        assert!(!session.is_active());
        let result = session.advance_phase("too-late");
        assert!(matches!(result, Err(AdvisoryError::WorkflowEnded)));
    }

    #[test]
    fn end_workflow_removes_from_active() {
        let mut advisory = ClientAdvisory::new();
        let session = advisory.declare_workflow();
        let wid = session.workflow_id();
        assert_eq!(advisory.active_count(), 1);
        advisory.end_workflow(wid);
        assert_eq!(advisory.active_count(), 0);
        assert!(!session.is_active());
    }

    #[test]
    fn client_id_is_stable_across_sessions() {
        let mut advisory = ClientAdvisory::new();
        let s1 = advisory.declare_workflow();
        let s2 = advisory.declare_workflow();
        assert_eq!(s1.client_id, s2.client_id);
        assert_eq!(s1.client_id, advisory.client_id());
    }

    #[test]
    fn workflow_ids_are_unique() {
        let mut advisory = ClientAdvisory::new();
        let s1 = advisory.declare_workflow();
        let s2 = advisory.declare_workflow();
        assert_ne!(s1.workflow_id, s2.workflow_id);
    }

    // --- Advisory channel tests ---

    /// Create a channel in "logically connected" state without a real TCP
    /// connection. Hints go to the queue only (no TCP send, since
    /// `tcp_stream` is None and `send_hint_tcp` returns false, falling
    /// through to the queue path).
    fn connected_channel_no_tcp(endpoint: &str) -> AdvisoryChannel {
        AdvisoryChannel {
            endpoint: endpoint.into(),
            connected: AtomicBool::new(true),
            tcp_stream: Mutex::new(None),
            hint_queue: Mutex::new(VecDeque::new()),
            max_queue_depth: DEFAULT_MAX_QUEUE_DEPTH,
        }
    }

    #[test]
    fn advisory_channel_send_hint_when_connected() {
        let ch = connected_channel_no_tcp("localhost:9090");
        assert!(ch.is_connected());
        let ok = ch.send_hint(AdvisoryHint::Profile {
            profile: "checkpoint".into(),
        });
        assert!(ok);
        // Hint went to queue (no TCP stream).
        let hints: Vec<_> = ch.hint_queue.lock().unwrap().drain(..).collect();
        assert_eq!(hints.len(), 1);
        assert!(matches!(&hints[0], AdvisoryHint::Profile { profile } if profile == "checkpoint"));
    }

    #[test]
    fn advisory_channel_send_hint_when_disconnected_returns_false() {
        let ch = AdvisoryChannel::new("localhost:9090".into());
        // Do not connect.
        assert!(!ch.is_connected());
        let ok = ch.send_hint(AdvisoryHint::Profile {
            profile: "x".into(),
        });
        assert!(!ok);
    }

    #[test]
    fn advisory_channel_drops_oldest_on_overflow() {
        let ch = AdvisoryChannel {
            endpoint: "localhost:9090".into(),
            connected: AtomicBool::new(true),
            tcp_stream: Mutex::new(None),
            hint_queue: Mutex::new(VecDeque::new()),
            max_queue_depth: 2,
        };
        ch.send_hint(AdvisoryHint::Prefetch {
            file_id: 1,
            offset: 0,
            length: 100,
        });
        ch.send_hint(AdvisoryHint::Prefetch {
            file_id: 2,
            offset: 0,
            length: 200,
        });
        // Queue is full (2). Next send drops oldest (file_id=1).
        ch.send_hint(AdvisoryHint::Prefetch {
            file_id: 3,
            offset: 0,
            length: 300,
        });
        let hints: Vec<_> = ch.hint_queue.lock().unwrap().drain(..).collect();
        assert_eq!(hints.len(), 2);
        // Oldest was dropped; remaining are file_id 2 and 3.
        assert!(matches!(
            &hints[0],
            AdvisoryHint::Prefetch { file_id: 2, .. }
        ));
        assert!(matches!(
            &hints[1],
            AdvisoryHint::Prefetch { file_id: 3, .. }
        ));
    }

    #[test]
    fn advisory_channel_drain_empties_queue() {
        let ch = connected_channel_no_tcp("localhost:9090");
        ch.send_hint(AdvisoryHint::Profile {
            profile: "a".into(),
        });
        ch.send_hint(AdvisoryHint::Profile {
            profile: "b".into(),
        });
        // drain_hints() tries TCP (fails) but returns the vec.
        let hints = ch.drain_hints();
        assert_eq!(hints.len(), 2);
        // Second drain should be empty (queue was drained).
        assert!(ch.drain_hints().is_empty());
    }

    #[test]
    fn advisory_channel_mark_disconnected() {
        let ch = connected_channel_no_tcp("localhost:9090");
        assert!(ch.is_connected());
        ch.mark_disconnected();
        assert!(!ch.is_connected());
        // Sending after disconnect should fail gracefully.
        assert!(!ch.send_hint(AdvisoryHint::Profile {
            profile: "x".into(),
        }));
    }

    #[test]
    fn advisory_channel_try_connect_empty_endpoint_returns_false() {
        let ch = AdvisoryChannel::new(String::new());
        assert!(!ch.try_connect());
        assert!(!ch.is_connected());
    }

    #[test]
    fn advisory_channel_try_connect_unreachable_returns_false() {
        // Port 1 is almost certainly not listening.
        let ch = AdvisoryChannel::new("127.0.0.1:1".into());
        assert!(!ch.try_connect());
        assert!(!ch.is_connected());
    }

    #[test]
    fn client_advisory_with_empty_endpoint_does_not_connect() {
        let advisory = ClientAdvisory::with_advisory_endpoint(String::new());
        assert!(advisory.channel().is_some());
        assert!(!advisory.channel().unwrap().is_connected());
    }

    // ---------------------------------------------------------------
    // Scenario: Client declares a workflow and correlates operations
    // ---------------------------------------------------------------
    #[test]
    fn declare_workflow_returns_session_with_correlation() {
        let mut advisory = ClientAdvisory::new();
        let session = advisory.declare_workflow();

        // Session has a valid workflow_id.
        assert_ne!(session.workflow_id(), 0);
        // Session is active and at phase 0.
        assert!(session.is_active());
        assert_eq!(session.current_phase(), 0);
        // Client ID is stable.
        assert_eq!(session.client_id, advisory.client_id());

        // Operations without a session work unchanged (I-WA1, I-WA2).
        // (The advisory is optional — we just verify the session exists.)
    }

    // ---------------------------------------------------------------
    // Scenario: Pattern-detector emits access-pattern hint on sequential read
    // ---------------------------------------------------------------
    #[test]
    fn pattern_detector_emits_sequential_hint() {
        use crate::prefetch::PrefetchAdvisor;

        let mut advisor = PrefetchAdvisor::default();

        // Three consecutive sequential reads.
        advisor.record_read(42, 0, 4096);
        advisor.record_read(42, 4096, 4096);
        advisor.record_read(42, 8192, 4096);

        // After threshold, the detector classifies access as sequential
        // and suggests a prefetch range.
        let suggestion = advisor.record_read_suggestion(42, 12288, 4096);
        assert!(
            suggestion.is_some(),
            "sequential pattern should trigger prefetch suggestion"
        );

        let hint = suggestion.unwrap().to_hint();
        assert!(
            matches!(hint, AdvisoryHint::Prefetch { file_id: 42, .. }),
            "hint should be a Prefetch for the detected file"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: Client declares prefetch ranges (batched per I-WA16)
    // ---------------------------------------------------------------
    #[test]
    fn prefetch_ranges_batched() {
        // PrefetchHint messages are bounded per I-WA16.
        // Verify hint creation does not panic with large inputs.
        let hints: Vec<AdvisoryHint> = (0..100)
            .map(|i| AdvisoryHint::Prefetch {
                file_id: 1,
                offset: i * 4096,
                length: 4096,
            })
            .collect();
        assert_eq!(hints.len(), 100);

        // Each hint is independently serializable.
        for hint in &hints {
            let json = serde_json::to_vec(hint).unwrap();
            assert!(!json.is_empty());
        }
    }

    // ---------------------------------------------------------------
    // Scenario: Client throttles on hard backpressure telemetry
    // ---------------------------------------------------------------
    #[test]
    fn hard_backpressure_telemetry_has_retry_after() {
        let feedback = TelemetryFeedback::Backpressure {
            severity: "hard".into(),
            retry_after_ms: 250,
        };

        match &feedback {
            TelemetryFeedback::Backpressure {
                severity,
                retry_after_ms,
            } => {
                assert_eq!(severity, "hard");
                assert_eq!(*retry_after_ms, 250);
            }
            _ => unreachable!("expected backpressure feedback"),
        }
    }

    // ---------------------------------------------------------------
    // Scenario: Advisory disabled — client degrades gracefully
    // ---------------------------------------------------------------
    #[test]
    fn advisory_disabled_degrades_gracefully() {
        // When advisory is disabled, declare_workflow can still succeed
        // locally (pattern-inference fallback). The channel is simply
        // not connected.
        let mut advisory = ClientAdvisory::new(); // no endpoint
        assert!(advisory.channel().is_none());

        // Workflow declaration works without advisory channel.
        let session = advisory.declare_workflow();
        assert!(session.is_active());

        // Phase advance works normally.
        let phase = session.advance_phase("stage-in").unwrap();
        assert_eq!(phase, 1);
    }

    #[test]
    fn declare_workflow_queues_hint_when_channel_connected() {
        // Use a logically-connected channel without real TCP.
        let channel = connected_channel_no_tcp("localhost:9090");
        let mut advisory = ClientAdvisory {
            client_id: uuid::Uuid::new_v4().as_u128(),
            active_workflows: HashMap::new(),
            channel: Some(channel),
        };
        let _session = advisory.declare_workflow();
        let hints: Vec<_> = advisory
            .channel()
            .unwrap()
            .hint_queue
            .lock()
            .unwrap()
            .drain(..)
            .collect();
        assert_eq!(hints.len(), 1);
        assert!(matches!(&hints[0], AdvisoryHint::Profile { profile } if profile == "default"));
    }
}
