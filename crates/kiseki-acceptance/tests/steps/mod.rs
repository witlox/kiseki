//! Step definitions for Kiseki acceptance tests.
//!
//! Only steps with REAL behavioral assertions are defined here.
//! Undefined steps show as "skipped" in cucumber output — that's
//! our honest backlog of unimplemented behavior.

pub mod advisory;
pub mod auth;
pub mod chunk;
pub mod client;
pub mod composition;
pub mod control;
pub mod crypto;
pub mod ec;
pub mod gateway;
pub mod helpers;
pub mod log;
pub mod operational;
pub mod view;
