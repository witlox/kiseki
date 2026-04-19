//! Server configuration.

use std::net::SocketAddr;

/// Server configuration — populated from environment or defaults.
pub struct ServerConfig {
    /// Address for the data-path gRPC listener.
    pub data_addr: SocketAddr,
    /// Address for the advisory gRPC listener (isolated runtime).
    pub advisory_addr: SocketAddr,
}

impl ServerConfig {
    /// Load config from environment variables with sensible defaults.
    pub fn from_env() -> Self {
        let data_addr = std::env::var("KISEKI_DATA_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9100".into())
            .parse()
            .expect("invalid KISEKI_DATA_ADDR");

        let advisory_addr = std::env::var("KISEKI_ADVISORY_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:9101".into())
            .parse()
            .expect("invalid KISEKI_ADVISORY_ADDR");

        Self {
            data_addr,
            advisory_addr,
        }
    }
}
