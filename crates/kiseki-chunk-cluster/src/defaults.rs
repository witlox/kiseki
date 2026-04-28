//! Per-cluster-size durability defaults (Phase 16b step 3).
//!
//! Picks `Replication-N` parameters + `min_acks` based on the
//! configured cluster size so every node boots with a coherent
//! durability posture without per-deployment tuning. Mirrors the
//! ADR-005 "Phase 16a default — Replication-3 below 6 nodes" table.
//!
//! Phase 16b ships *Replication-N only*. The "EC 4+2 candidate" entry
//! in ADR-005 lands in step 6; until then ≥6-node clusters still get
//! Replication-3 — same correctness, sub-optimal storage tax.

/// Durability defaults derived from cluster size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClusterDurabilityDefaults {
    /// Number of copies (Replication-N). 1 = local-only.
    pub copies: u8,
    /// Minimum acks required to consider a write durable.
    /// `local + remote_acks ≥ min_acks` ⇒ success.
    pub min_acks: usize,
}

/// Pick durability defaults for a cluster of `size` nodes.
///
/// | Size  | Strategy       | min_acks | Notes                             |
/// |-------|----------------|----------|-----------------------------------|
/// | 0, 1  | local-only     | 1        | D-6 single-node compat            |
/// | 2     | Replication-2  | 2        | both nodes ack; no failure tolerance |
/// | 3-5   | Replication-3  | 2        | I-L2 majority semantics           |
/// | ≥6    | Replication-3  | 2        | EC 4+2 lands in step 6            |
#[must_use]
pub fn defaults_for(cluster_size: usize) -> ClusterDurabilityDefaults {
    match cluster_size {
        0 | 1 => ClusterDurabilityDefaults {
            copies: 1,
            min_acks: 1,
        },
        2 => ClusterDurabilityDefaults {
            copies: 2,
            min_acks: 2,
        },
        _ => ClusterDurabilityDefaults {
            copies: 3,
            min_acks: 2,
        },
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
    }

    #[test]
    fn zero_node_input_degenerates_to_local_only() {
        // Defensive: should never happen at runtime, but the function
        // must not panic / divide-by-zero when wired before topology
        // discovery completes.
        let d = defaults_for(0);
        assert_eq!(d.copies, 1);
        assert_eq!(d.min_acks, 1);
    }

    #[test]
    fn two_node_cluster_uses_replication_two() {
        let d = defaults_for(2);
        assert_eq!(d.copies, 2);
        assert_eq!(
            d.min_acks, 2,
            "Replication-2 has no failure tolerance during writes — both peers ack"
        );
    }

    #[test]
    fn three_node_cluster_is_the_canonical_replication_three() {
        let d = defaults_for(3);
        assert_eq!(d.copies, 3);
        assert_eq!(d.min_acks, 2, "I-L2 majority: 2-of-3 quorum");
    }

    /// 4-5-node clusters keep Replication-3 (no EC in 16b step 3).
    /// Each chunk lands on 3 of the available nodes — extra nodes are
    /// candidates for placement diversity / future EC.
    #[test]
    fn small_clusters_4_to_5_keep_replication_three() {
        for size in [4usize, 5] {
            let d = defaults_for(size);
            assert_eq!(d.copies, 3, "size={size} keeps Rep-3");
            assert_eq!(d.min_acks, 2, "size={size} keeps 2-ack quorum");
        }
    }

    /// ≥6-node clusters: today still Rep-3 (EC 4+2 candidate from
    /// ADR-005 lands in step 6). Test pins this so we *notice* when
    /// step 6 changes the answer — the test assertion will need an
    /// update, signaling that EC is live.
    #[test]
    fn large_clusters_still_replication_three_pre_step_6() {
        let d = defaults_for(6);
        assert_eq!(d.copies, 3);
        assert_eq!(d.min_acks, 2);
        let d10 = defaults_for(10);
        assert_eq!(d10.copies, 3);
    }
}
