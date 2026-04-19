//! Workflow Advisory runtime for Kiseki.
//!
//! Manages workflow declarations, hint routing, budget enforcement,
//! and telemetry feedback. Runs on an isolated tokio runtime with a
//! separate gRPC listener (ADR-021 §1). No data-path code — this
//! crate is never a dependency of data-path crates (I-WA2).
//!
//! Invariant mapping:
//!   - I-WA1 — data-path equivalence with/without advisory
//!   - I-WA2 — advisory failure never blocks data path
//!   - I-WA5 — k-anonymity in aggregate telemetry
//!   - I-WA6, I-WA15 — `ScopeNotFound` indistinguishable from unauthorized
//!   - I-WA8 — audit event batching guarantees
//!   - I-WA13 — phase monotonicity

#![deny(unsafe_code)]

pub mod budget;
pub mod error;
pub mod lookup;
pub mod workflow;

pub use budget::BudgetEnforcer;
pub use error::AdvisoryError;
pub use lookup::AdvisoryLookup;
pub use workflow::{WorkflowEntry, WorkflowTable};
