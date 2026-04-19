//! Kiseki storage server — composes all Rust crates into a single binary.
//!
//! Phase 12 integration binary. In production this will:
//! - Compose all data-path contexts (Log, Chunk, Composition, View)
//! - Start the Transport listener (TCP+TLS with mTLS)
//! - Start Protocol Gateways (NFS, S3)
//! - Start the Advisory runtime on an isolated tokio runtime
//! - Run the discovery responder
//! - Report node health (clock quality, device health)
//!
//! Currently a scaffold that validates all crate dependencies resolve
//! and prints version info.

// Binary crate: allow expect/unwrap for startup and top-level error handling.
#![allow(clippy::expect_used)]

fn main() {
    // Validate all crate imports resolve (compile-time integration check).
    let _ = kiseki_common::time::ClockQuality::Ntp;
    let _ = kiseki_crypto::aead::Aead::new();
    let _ = kiseki_log::shard::ShardState::Healthy;
    let _ = kiseki_keymanager::health::KeyManagerStatus::Healthy;
    let _ = kiseki_audit::event::AuditEventType::DataRead;
    let _ = kiseki_chunk::pool::DurabilityStrategy::default();
    let _ = kiseki_composition::multipart::MultipartState::InProgress;
    let _ = kiseki_view::descriptor::ProtocolSemantics::Posix;
    let _ = kiseki_gateway::error::GatewayError::OperationNotSupported(String::new());
    let _ = kiseki_client::cache::ClientCache::new(5000, 100);
    let _ = kiseki_advisory::workflow::WorkflowTable::new();

    eprintln!(
        "kiseki-server: all {} crates linked successfully. \
         Full server startup not yet implemented (Phase 12 scaffold).",
        12
    );
}
