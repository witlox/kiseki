//! Admin CLI types and handlers.
//!
//! Defines data types for admin operations. The actual CLI binary/parsing
//! is separate; this module provides the domain types and formatting.

/// Admin operation result.
#[allow(dead_code)] // Used when admin gRPC wiring lands.
pub struct AdminResponse {
    /// Whether the operation succeeded.
    pub success: bool,
    /// Human-readable message.
    pub message: String,
    /// Optional JSON payload for structured output.
    pub data: Option<String>,
}

/// Cluster status summary.
pub struct ClusterStatus {
    /// Number of nodes in the cluster.
    pub node_count: usize,
    /// Number of shards across all pools.
    pub shard_count: usize,
    /// Number of storage pools.
    pub pool_count: usize,
    /// Total capacity in bytes.
    pub total_capacity_bytes: u64,
    /// Used capacity in bytes.
    pub used_capacity_bytes: u64,
    /// Whether the cluster is in maintenance mode.
    pub maintenance_mode: bool,
}

impl ClusterStatus {
    /// Format as a human-readable table.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // percentage display; precision loss acceptable
    pub fn to_table(&self) -> String {
        let used_pct = if self.total_capacity_bytes == 0 {
            0.0
        } else {
            (self.used_capacity_bytes as f64 / self.total_capacity_bytes as f64) * 100.0
        };

        format!(
            "Cluster Status\n\
             ──────────────────────────────\n\
             Nodes:            {}\n\
             Shards:           {}\n\
             Pools:            {}\n\
             Total capacity:   {} bytes\n\
             Used capacity:    {} bytes ({used_pct:.1}%)\n\
             Maintenance mode: {}",
            self.node_count,
            self.shard_count,
            self.pool_count,
            self.total_capacity_bytes,
            self.used_capacity_bytes,
            if self.maintenance_mode { "ON" } else { "OFF" },
        )
    }

    /// Serialize to JSON string.
    ///
    /// Uses manual formatting to avoid a `serde_json` dependency in
    /// the server binary for this single use case.
    #[must_use]
    #[allow(dead_code)] // Used when admin gRPC wiring lands.
    pub fn to_json(&self) -> String {
        format!(
            "{{\
            \"node_count\":{},\
            \"shard_count\":{},\
            \"pool_count\":{},\
            \"total_capacity_bytes\":{},\
            \"used_capacity_bytes\":{},\
            \"maintenance_mode\":{}\
            }}",
            self.node_count,
            self.shard_count,
            self.pool_count,
            self.total_capacity_bytes,
            self.used_capacity_bytes,
            self.maintenance_mode,
        )
    }
}

/// Aggregate cluster status from in-memory state.
///
/// Returns a default (empty) status. In production this will query the
/// control plane and storage pool managers for live data.
#[must_use]
pub fn cluster_status() -> ClusterStatus {
    // TODO: wire to actual cluster state once control-plane integration lands.
    ClusterStatus {
        node_count: 0,
        shard_count: 0,
        pool_count: 0,
        total_capacity_bytes: 0,
        used_capacity_bytes: 0,
        maintenance_mode: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_format_includes_all_fields() {
        let status = ClusterStatus {
            node_count: 3,
            shard_count: 12,
            pool_count: 2,
            total_capacity_bytes: 1_000_000,
            used_capacity_bytes: 250_000,
            maintenance_mode: false,
        };
        let table = status.to_table();
        assert!(table.contains("Nodes:            3"), "table: {table}");
        assert!(table.contains("Shards:           12"), "table: {table}");
        assert!(table.contains("Pools:            2"), "table: {table}");
        assert!(table.contains("1000000"), "table: {table}");
        assert!(table.contains("250000"), "table: {table}");
        assert!(table.contains("25.0%"), "table: {table}");
        assert!(table.contains("OFF"), "table: {table}");
    }

    #[test]
    fn table_format_maintenance_on() {
        let status = ClusterStatus {
            node_count: 1,
            shard_count: 4,
            pool_count: 1,
            total_capacity_bytes: 0,
            used_capacity_bytes: 0,
            maintenance_mode: true,
        };
        let table = status.to_table();
        assert!(table.contains("ON"), "table: {table}");
    }

    #[test]
    fn json_roundtrip() {
        let status = ClusterStatus {
            node_count: 5,
            shard_count: 20,
            pool_count: 3,
            total_capacity_bytes: 2_000_000,
            used_capacity_bytes: 500_000,
            maintenance_mode: true,
        };
        let json = status.to_json();
        assert!(json.contains("\"node_count\":5"), "json: {json}");
        assert!(json.contains("\"shard_count\":20"), "json: {json}");
        assert!(json.contains("\"pool_count\":3"), "json: {json}");
        assert!(
            json.contains("\"total_capacity_bytes\":2000000"),
            "json: {json}"
        );
        assert!(
            json.contains("\"used_capacity_bytes\":500000"),
            "json: {json}"
        );
        assert!(json.contains("\"maintenance_mode\":true"), "json: {json}");
    }

    #[test]
    fn zero_capacity_percentage() {
        let status = ClusterStatus {
            node_count: 0,
            shard_count: 0,
            pool_count: 0,
            total_capacity_bytes: 0,
            used_capacity_bytes: 0,
            maintenance_mode: false,
        };
        let table = status.to_table();
        assert!(table.contains("0.0%"), "table: {table}");
    }

    #[test]
    fn cluster_status_returns_defaults() {
        let s = cluster_status();
        assert_eq!(s.node_count, 0);
        assert!(!s.maintenance_mode);
    }
}
