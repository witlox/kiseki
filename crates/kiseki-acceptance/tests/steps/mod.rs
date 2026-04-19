//! Step definitions for Kiseki acceptance tests.
//!
//! Each module registers steps for a specific feature file.
//! cucumber-rs macros register globally via the World type.
//!
//! Modules without real crate code behind them are stubs —
//! unmatched scenarios show as "skipped" until implementations exist.

pub mod helpers;
pub mod log;
