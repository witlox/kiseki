//! Step definitions for Kiseki acceptance tests.
//!
//! Only steps with REAL behavioral assertions are defined here.
//! Undefined steps show as "skipped" in cucumber output — that's
//! our honest backlog of unimplemented behavior.

pub mod admin;
pub mod advisory;
pub mod auth;
pub mod block;
pub mod chunk;
pub mod client;
pub mod cluster;
pub mod composition;
pub mod control;
pub mod crypto;
pub mod device;
pub mod ec;
pub mod gateway;
pub mod helpers;
pub mod kms;
pub mod log;
pub mod operational;
pub mod protocol;
pub mod raft;
pub mod small_file;
pub mod view;
