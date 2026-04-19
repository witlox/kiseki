//! Property tests for `HybridLogicalClock`.
//!
//! Verifies the load-bearing invariants from I-T5, I-T7, and
//! `ubiquitous-language.md#HLC`:
//!
//! 1. `tick` is strictly monotonic: every returned clock is > the input.
//! 2. `merge` returns a clock strictly greater than **both** inputs in
//!    the induced total order (Lamport rule).
//! 3. The induced total order is well-defined across nodes and consistent
//!    with `Ord::cmp` on `(physical, logical, node_id)`.
//! 4. The `node_id` final tiebreaker makes equal `(physical, logical)`
//!    clocks from different nodes comparable without panics.

use kiseki_common::ids::NodeId;
use kiseki_common::time::HybridLogicalClock;

use proptest::prelude::*;

fn hlc_strategy() -> impl Strategy<Value = HybridLogicalClock> {
    (any::<u64>(), any::<u32>(), any::<u64>()).prop_map(|(p, l, n)| HybridLogicalClock {
        physical_ms: p,
        logical: l,
        node_id: NodeId(n),
    })
}

proptest! {
    /// `tick` never produces a clock ≤ its input.
    #[test]
    fn tick_is_strictly_monotonic(
        hlc in hlc_strategy(),
        now in any::<u64>(),
    ) {
        let next = hlc.tick(now);
        prop_assert!(next > hlc, "tick({hlc:?}, now={now}) = {next:?} must be > input");
    }

    /// Repeated `tick` calls produce a strictly increasing sequence.
    #[test]
    fn repeated_tick_is_strictly_increasing(
        start in hlc_strategy(),
        nows in proptest::collection::vec(any::<u64>(), 2..32),
    ) {
        let mut current = start;
        for now in nows {
            let next = current.tick(now);
            prop_assert!(next > current);
            current = next;
        }
    }

    /// `merge` returns a clock strictly greater than both inputs.
    #[test]
    fn merge_dominates_both_inputs(
        local in hlc_strategy(),
        remote in hlc_strategy(),
        now in any::<u64>(),
    ) {
        let merged = local.merge(remote, now);
        // The merged clock takes its node_id from `local`, so direct
        // ordering vs `remote` may tie on (phys, logical) if the remote
        // node happens to compare-greater. Compare on the three-tuple
        // (phys, logical) projection: merged must dominate both in the
        // lexicographic (phys, logical) order even ignoring node_id.
        let merged_key = (merged.physical_ms, merged.logical);
        let local_key  = (local.physical_ms,  local.logical);
        let remote_key = (remote.physical_ms, remote.logical);
        prop_assert!(
            merged_key > local_key,
            "merged {merged:?} (key={merged_key:?}) must dominate local {local:?} (key={local_key:?})"
        );
        prop_assert!(
            merged_key > remote_key,
            "merged {merged:?} (key={merged_key:?}) must dominate remote {remote:?} (key={remote_key:?})"
        );
    }

    /// Merging with a clock that is already ≤ local + a zero `now`
    /// still strictly advances local (logical increment).
    #[test]
    fn merge_with_stale_remote_advances_logical(
        local in hlc_strategy(),
    ) {
        let stale_remote = HybridLogicalClock {
            physical_ms: local.physical_ms.saturating_sub(1),
            logical: 0,
            node_id: NodeId(0xdead_beef),
        };
        let merged = local.merge(stale_remote, 0);
        prop_assert!(
            (merged.physical_ms, merged.logical) > (local.physical_ms, local.logical),
            "merged {merged:?} must strictly advance local {local:?}"
        );
    }

    /// `Ord` is a total order: reflexive, antisymmetric, transitive.
    #[test]
    fn ord_is_total(
        a in hlc_strategy(),
        b in hlc_strategy(),
        c in hlc_strategy(),
    ) {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let ab = a.cmp(&b);
        let ba = b.cmp(&a);
        // Antisymmetry + totality.
        prop_assert!(
            matches!((ab, ba), (Less, Greater) | (Greater, Less) | (Equal, Equal)),
            "cmp(a,b)={ab:?} but cmp(b,a)={ba:?}"
        );
        // Transitivity.
        if a < b && b < c {
            prop_assert!(a < c);
        }
        // Reflexivity of Eq on PartialEq.
        prop_assert_eq!(a, a);
    }

    /// Two clocks from different nodes with identical (phys, logical)
    /// compare by node_id and never cause a tie in the total order.
    #[test]
    fn node_id_breaks_ties(
        phys in any::<u64>(),
        logical in any::<u32>(),
        node_a in any::<u64>(),
        node_b in any::<u64>(),
    ) {
        prop_assume!(node_a != node_b);
        let a = HybridLogicalClock { physical_ms: phys, logical, node_id: NodeId(node_a) };
        let b = HybridLogicalClock { physical_ms: phys, logical, node_id: NodeId(node_b) };
        prop_assert_ne!(a.cmp(&b), std::cmp::Ordering::Equal);
    }
}
