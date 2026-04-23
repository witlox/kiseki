//! Client-side workflow advisory integration (ADR-020).
//!
//! Provides [`WorkflowSession`] for tracking multi-phase workflows and
//! [`ClientAdvisory`] for managing the set of active sessions within a
//! single client process. Workflow and client identifiers are 128-bit
//! values drawn from a CSPRNG (uuid v4).

use std::collections::HashMap;
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
}

impl ClientAdvisory {
    /// Create a new `ClientAdvisory` with a CSPRNG-generated client id.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client_id: uuid::Uuid::new_v4().as_u128(),
            active_workflows: HashMap::new(),
        }
    }

    /// Declare a new workflow, returning a shared handle to the session.
    pub fn declare_workflow(&mut self) -> Arc<WorkflowSession> {
        let session = Arc::new(WorkflowSession::new(self.client_id));
        self.active_workflows
            .insert(session.workflow_id, Arc::clone(&session));
        session
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
}
