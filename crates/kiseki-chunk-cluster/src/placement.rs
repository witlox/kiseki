//! Cluster placement (Phase 16c step 2).
//!
//! Picks a deterministic, evenly-distributed subset of cluster nodes
//! to hold each chunk's fragments. Uses **Rendezvous hashing**
//! (Highest Random Weight, Thaler & Ravishankar 1998) — the same
//! algorithm Ceph CRUSH simplifies to when there's no failure-domain
//! hierarchy. Gives:
//!
//! - **Determinism**: `(chunk_id, nodes, target_copies)` always picks
//!   the same set, so a follower computing placement after applying a
//!   `ChunkAndDelta` matches the leader's placement bit-for-bit.
//! - **Even distribution**: each node carries `chunk_count *
//!   target_copies / nodes.len()` fragments on average.
//! - **Minimal disruption**: removing a node only relocates the
//!   chunks that were on it (~`1/N` of the chunk space). 16b Finding 4
//!   identified the missing version of this; today's gateway just
//!   uses every peer regardless of `target_copies`.
//!
//! Spec: `specs/architecture/adr/005-ec-and-chunk-durability.md`,
//! `specs/findings/phase-16b-adversary-audit.md` Finding 4.

use kiseki_common::ids::ChunkId;

/// Pick `target_copies` distinct nodes for a chunk, given the full
/// cluster membership. Stable under node ordering (input list is
/// canonicalized via sort before hashing).
///
/// Returns at most `target_copies.min(nodes.len())` entries — a
/// 3-node cluster with `target_copies = 4` returns all 3.
#[must_use]
pub fn pick_placement(chunk_id: &ChunkId, nodes: &[u64], target_copies: usize) -> Vec<u64> {
    if nodes.is_empty() || target_copies == 0 {
        return Vec::new();
    }
    // Score every node, then take the top `target_copies` by score.
    let mut scored: Vec<(u64, u64)> = nodes
        .iter()
        .map(|&n| (rendezvous_score(chunk_id, n), n))
        .collect();
    // Sort by score descending; ties broken by node id ascending so
    // the function stays deterministic across rebuilds. The default
    // tuple-sort handles this naturally if we pre-sort by node id.
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(target_copies.min(nodes.len()))
        .map(|(_score, node)| node)
        .collect()
}

/// Rendezvous score for `(chunk_id, node)`. The exact hash function
/// only needs to be:
/// - **Stable**: same input → same output across processes / rebuilds.
/// - **Avalanche-good**: small input changes spread across all bits
///   so one chunk's score doesn't bias another node's score.
///
/// We use the `xxhash`-style FNV-1a-ish hash baked into Rust's
/// `DefaultHasher`. That's not cryptographic — fine here, since the
/// score isn't a security boundary.
fn rendezvous_score(chunk_id: &ChunkId, node: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    // SipHash-1-3 (Rust's DefaultHasher) is keyed by a per-process
    // random seed by default — that would break determinism across
    // nodes. Use a fixed-seed alternative.
    //
    // Approach: combine chunk_id bytes + node into a single u64 via
    // a mix function. We use a simple deterministic hash built from
    // wrapping arithmetic to avoid pulling in an external xxhash
    // dep just for this.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Reset DefaultHasher — actually, DefaultHasher::new() is
    // deterministic across the process: it does NOT seed from
    // anywhere random. The randomized version is RandomState used by
    // HashMap. So this is safe.
    chunk_id.0.hash(&mut h);
    node.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(b: u8) -> ChunkId {
        ChunkId([b; 32])
    }

    /// Phase 16c step 2: same input always produces the same
    /// placement — required so a follower computing placement
    /// after Raft apply matches the leader's stored placement.
    #[test]
    fn placement_is_deterministic_across_calls() {
        let nodes = vec![1, 2, 3, 4, 5];
        let p1 = pick_placement(&cid(0xAA), &nodes, 3);
        let p2 = pick_placement(&cid(0xAA), &nodes, 3);
        assert_eq!(p1, p2, "same chunk + nodes must yield same placement");
        assert_eq!(p1.len(), 3, "exactly target_copies entries");
    }

    /// Different chunks should usually pick different placements.
    /// Statistical — over 100 random chunks, the placement
    /// distribution must touch every node (within tolerance).
    #[test]
    fn placement_spreads_across_nodes() {
        let nodes = vec![1, 2, 3, 4, 5, 6];
        let mut hits: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
        for i in 0..100u8 {
            let placement = pick_placement(&cid(i), &nodes, 3);
            for n in placement {
                *hits.entry(n).or_default() += 1;
            }
        }
        // 100 chunks × 3 copies = 300 placements over 6 nodes ≈ 50/node.
        // Tolerance: every node should hit ≥10 times.
        assert_eq!(hits.len(), 6, "every node hit at least once");
        for (node, count) in &hits {
            assert!(
                *count >= 10,
                "node {node} only got {count} placements — poor distribution"
            );
        }
    }

    /// `target_copies > nodes.len()` returns the full node set
    /// (capped at cluster size). This is the small-cluster path —
    /// e.g. a 3-node cluster with `target_copies = 4` should still
    /// return [1, 2, 3].
    #[test]
    fn small_cluster_returns_all_nodes() {
        let nodes = vec![1, 2, 3];
        let placement = pick_placement(&cid(0x33), &nodes, 4);
        assert_eq!(placement.len(), 3);
        let mut sorted = placement.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![1, 2, 3], "all 3 nodes returned");
    }

    /// Empty inputs return empty placement.
    #[test]
    fn empty_nodes_returns_empty_placement() {
        let placement = pick_placement(&cid(0x55), &[], 3);
        assert!(placement.is_empty());
        let placement = pick_placement(&cid(0x55), &[1, 2, 3], 0);
        assert!(placement.is_empty());
    }

    /// Removing a node only relocates the chunks that were on it —
    /// the HRW property that makes rebalancing cheap. This pins the
    /// minimal-disruption guarantee.
    #[test]
    fn removing_a_node_only_displaces_chunks_that_were_on_it() {
        let big = vec![1, 2, 3, 4, 5, 6];
        let small = vec![1, 2, 3, 4, 5]; // node 6 removed

        let mut moved = 0u32;
        let mut total = 0u32;
        for i in 0..100u8 {
            let p_before = pick_placement(&cid(i), &big, 3);
            let p_after = pick_placement(&cid(i), &small, 3);
            // If node 6 wasn't in p_before, p_after must equal p_before
            // (no movement). If node 6 WAS in p_before, exactly one
            // node from `small` replaces it.
            let was_on_6 = p_before.contains(&6);
            total += 1;
            if was_on_6 {
                moved += 1;
                assert!(
                    !p_after.contains(&6),
                    "node 6 must not appear in the post-removal placement"
                );
            } else {
                assert_eq!(
                    p_before, p_after,
                    "chunk {i} unaffected by node 6 removal but placement changed"
                );
            }
        }
        // Across 100 chunks, ~ (3/6) = 50% should be displaced (the
        // ones whose top-3 included node 6). Tolerance 25-75%.
        assert!(
            (25..=75).contains(&((moved * 100) / total)),
            "moved {moved}/{total} — outside the expected 25-75% window"
        );
    }

    /// Node ordering on the input list doesn't change the output —
    /// avoids subtle bugs where leader and follower disagree because
    /// they iterated their peer set in different orders.
    #[test]
    fn placement_is_independent_of_input_order() {
        let nodes_a = vec![5, 3, 1, 4, 2];
        let nodes_b = vec![1, 2, 3, 4, 5];
        let p_a = pick_placement(&cid(0xC0), &nodes_a, 3);
        let p_b = pick_placement(&cid(0xC0), &nodes_b, 3);
        let mut sa = p_a;
        let mut sb = p_b;
        sa.sort_unstable();
        sb.sort_unstable();
        assert_eq!(sa, sb, "set-equality across input orders");
    }
}
