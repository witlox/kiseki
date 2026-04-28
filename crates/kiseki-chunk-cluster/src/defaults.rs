//! Per-cluster-size durability defaults (Phase 16b step 3, finalized
//! Phase 16e step 1).
//!
//! Picks the durability strategy + `min_acks` based on the configured
//! cluster size so every node boots with a coherent posture matching
//! ADR-005's defaults table. EC is the primary durability mode for
//! production-scale (≥6-node) clusters per **I-C4** ("EC is the
//! default") and **I-D1** ("repaired from EC parity"). Replication-N
//! is used for small clusters where I-D4's distinct-failure-domain
//! requirement can't be met (EC X+Y needs ≥X+Y nodes).

use crate::ec::EcStrategy;

/// Durability defaults derived from cluster size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClusterDurabilityDefaults {
    /// Number of fragments per chunk (`copies` for Replication,
    /// `data + parity` for EC). Equals
    /// `strategy.total_fragments()`.
    pub copies: u8,
    /// Minimum acks required to consider a write durable.
    /// `local + remote_acks ≥ min_acks` ⇒ success.
    pub min_acks: usize,
    /// Phase 16e step 1: full strategy (Replication-N or EC X+Y).
    /// Runtimes thread this into [`crate::ClusterCfg::ec_strategy`]
    /// so write/read dispatch routes through the right data path.
    pub strategy: EcStrategy,
}

/// Pick durability defaults for a cluster of `size` nodes.
///
/// | Size  | Strategy       | min_acks | Notes                                  |
/// |-------|----------------|----------|----------------------------------------|
/// | 0, 1  | Replication-1  | 1        | local-only (D-6 single-node compat)    |
/// | 2     | Replication-2  | 2        | both nodes ack; no failure tolerance   |
/// | 3-5   | Replication-3  | 2        | I-L2 majority; EC 4+2 needs 6 nodes    |
/// | ≥6    | EC 4+2         | 4        | I-C4 default; matches ADR-005 fast-nvme |
#[must_use]
pub fn defaults_for(cluster_size: usize) -> ClusterDurabilityDefaults {
    match cluster_size {
        0 | 1 => ClusterDurabilityDefaults {
            copies: 1,
            min_acks: 1,
            strategy: EcStrategy::Replication { copies: 1 },
        },
        2 => ClusterDurabilityDefaults {
            copies: 2,
            min_acks: 2,
            strategy: EcStrategy::Replication { copies: 2 },
        },
        3..=5 => ClusterDurabilityDefaults {
            copies: 3,
            min_acks: 2,
            strategy: EcStrategy::Replication { copies: 3 },
        },
        _ => {
            // ≥6 nodes: EC 4+2 — primary durability mode per I-C4.
            // min_acks = data shards (4) so a write is durable iff at
            // least the data fragments are placed; the 2 parity
            // fragments raise the failure tolerance to 2 missing
            // peers without blocking writes when only 1 is down.
            let strategy = EcStrategy::Ec { data: 4, parity: 2 };
            ClusterDurabilityDefaults {
                copies: u8::try_from(strategy.total_fragments()).unwrap_or(6),
                min_acks: 4,
                strategy,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node_is_local_only_no_quorum() {
        let d = defaults_for(1);
        assert_eq!(d.copies, 1);
        assert_eq!(
            d.min_acks, 1,
            "1-node cluster must not require >1 ack — would deadlock all writes"
        );
        assert_eq!(d.strategy, EcStrategy::Replication { copies: 1 });
    }

    #[test]
    fn zero_node_input_degenerates_to_local_only() {
        // Defensive: should never happen at runtime, but the function
        // must not panic / divide-by-zero when wired before topology
        // discovery completes.
        let d = defaults_for(0);
        assert_eq!(d.copies, 1);
        assert_eq!(d.min_acks, 1);
        assert_eq!(d.strategy, EcStrategy::Replication { copies: 1 });
    }

    #[test]
    fn two_node_cluster_uses_replication_two() {
        let d = defaults_for(2);
        assert_eq!(d.copies, 2);
        assert_eq!(
            d.min_acks, 2,
            "Replication-2 has no failure tolerance during writes — both peers ack"
        );
        assert_eq!(d.strategy, EcStrategy::Replication { copies: 2 });
    }

    #[test]
    fn three_node_cluster_is_the_canonical_replication_three() {
        let d = defaults_for(3);
        assert_eq!(d.copies, 3);
        assert_eq!(d.min_acks, 2, "I-L2 majority: 2-of-3 quorum");
        assert_eq!(d.strategy, EcStrategy::Replication { copies: 3 });
    }

    /// 4-5-node clusters keep Replication-3. Each chunk lands on 3
    /// of the available nodes — extra nodes are candidates for
    /// placement diversity / future EC. EC 4+2 needs 6 distinct
    /// failure domains (I-D4) which 4- or 5-node clusters can't
    /// provide.
    #[test]
    fn small_clusters_4_to_5_keep_replication_three() {
        for size in [4usize, 5] {
            let d = defaults_for(size);
            assert_eq!(d.copies, 3, "size={size} keeps Rep-3");
            assert_eq!(d.min_acks, 2, "size={size} keeps 2-ack quorum");
            assert_eq!(d.strategy, EcStrategy::Replication { copies: 3 });
        }
    }

    /// Phase 16e step 1: ≥6-node clusters get EC 4+2 by default.
    /// Matches ADR-005 fast-nvme + honors I-C4 ("EC is the default")
    /// and I-D1 ("repaired from EC parity"). Production HPC/AI
    /// clusters land here.
    #[test]
    fn six_node_cluster_uses_ec_four_two() {
        let d = defaults_for(6);
        assert_eq!(d.strategy, EcStrategy::Ec { data: 4, parity: 2 });
        assert_eq!(
            d.copies, 6,
            "EC 4+2 spreads across 6 distinct failure domains (I-D4)"
        );
        assert_eq!(
            d.min_acks, 4,
            "min_acks = data shards: a write is durable when ≥X fragments land"
        );
    }

    /// Larger clusters keep EC 4+2 today — pool-aware defaults
    /// (8+3 for bulk-nvme) ride on ADR-005's per-pool config which
    /// is configured by the cluster admin, not the size table.
    #[test]
    fn large_clusters_keep_ec_four_two_default() {
        for size in [10usize, 20, 100] {
            let d = defaults_for(size);
            assert_eq!(d.strategy, EcStrategy::Ec { data: 4, parity: 2 });
            assert_eq!(d.min_acks, 4);
        }
    }
}
