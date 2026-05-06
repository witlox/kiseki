//! Client-side per-edge binding selection (ADR-042 §3.2).
//!
//! Server-side `BindingSelector` (kiseki-transport::native::selector)
//! decides which bindings the LOCAL node spawns. The client side is
//! the parallel: each `(client → remote-node)` connection picks
//! independently from the bindings the remote node advertises in
//! its `NodeBindings`. A multi-node operation may use libfabric/cxi
//! to nodes 1–4 (Slingshot peers) and TCP-framed to nodes 5–10
//! (commodity peers) within the same session.
//!
//! Selection rule (§3.2.4): "highest-ranked latency_class mutually
//! supported by (a) the local environment, (b) the bindings that
//! node advertises". `Rdma > Low > Standard`. Operator pin
//! (`KISEKI_NATIVE_TRANSPORT`) collapses the choice to the pinned
//! binding when present; pin-mismatch returns `None` (caller gets
//! `PinnedBindingUnavailable` from the call site).
//!
//! `Draining` nodes (§1.7 state table): existing leases dial them
//! for in-flight work; new connections skip them. The selector
//! takes the [`NodeBindings::state`] into account — a `Failed` /
//! `Evicted` node returns `None` regardless of advertised bindings,
//! and a `Draining` node only matches when the caller's
//! `for_in_flight_work` flag is `true`.

use std::collections::BTreeSet;

use kiseki_proto::native_contract::{
    BindingEndpoint, BindingId, LatencyClass, NodeBindings, NodeState,
};
use kiseki_transport::native::OperatorPin;

/// Outcome of one edge-selection call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeSelection {
    /// Use this binding endpoint for the (client → node) connection.
    Match(BindingEndpoint),
    /// No mutually-supported binding. Caller routes to a different
    /// shard leader (per §3.3 mixed-NIC clusters) or surfaces
    /// `Unavailable { no_mutual_binding }` to the user.
    NoMatch {
        /// Why no match — populated for diagnostics. Distinguishes
        /// "node has no compatible binding" from "operator pin
        /// excludes everything this node serves" so client logs
        /// surface the right operator action.
        reason: NoMatchReason,
    },
}

/// Why edge selection returned no match. Mapped 1:1 from logs +
/// metrics so dashboards distinguish operator-pin misconfig from
/// genuine mixed-NIC heterogeneity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoMatchReason {
    /// The node is `Failed` or `Evicted`. Don't dial.
    NodeUnreachable,
    /// The node is `Draining` and the caller didn't request
    /// in-flight-work routing — new connections must skip per
    /// §1.7's state table.
    NodeDraining,
    /// No binding the client supports overlaps with the node's
    /// advertised set. Genuine mixed-NIC heterogeneity (§3.3).
    NoCompatibleBinding,
    /// `KISEKI_NATIVE_TRANSPORT` pinned to a binding this node
    /// doesn't advertise (or that the client doesn't support
    /// locally). Operators can scope the pin to specific processes.
    PinnedBindingNotAdvertised,
}

/// Inputs the caller already knows about the local environment.
/// Built once at `NativeClient` construction (probe local libs +
/// NIC) and reused across every edge selection.
#[derive(Clone, Debug)]
pub struct LocalCapabilities {
    /// Bindings the local environment can actually use (set
    /// membership; ordering doesn't matter for the selector).
    /// Built from kiseki-transport::native::BindingSelector's plan
    /// or a client-side equivalent that probes locally.
    pub supported: BTreeSet<BindingId>,
}

impl LocalCapabilities {
    /// Build from an iterable of supported bindings.
    #[must_use]
    pub fn from_iter<I: IntoIterator<Item = BindingId>>(it: I) -> Self {
        Self {
            supported: it.into_iter().collect(),
        }
    }
}

/// Pick the best binding for an edge to `target`. Pure function —
/// stateless, no side effects. Caller decides what to do with the
/// outcome (dial, route to another shard, surface to user).
///
/// `for_in_flight_work` controls draining-node behavior:
/// - `false` (default): skip `Draining` nodes (new connections only).
/// - `true`: dial `Draining` nodes IF the caller has an existing
///   lease/handle there; matches §1.7's "in-flight only" rule and
///   §9.1's lease-tracker behavior.
#[must_use]
pub fn select_for_edge(
    target: &NodeBindings,
    local: &LocalCapabilities,
    pin: OperatorPin,
    for_in_flight_work: bool,
) -> EdgeSelection {
    // Reachability gate per §1.7 state table.
    match target.state {
        NodeState::Failed | NodeState::Evicted => {
            return EdgeSelection::NoMatch {
                reason: NoMatchReason::NodeUnreachable,
            };
        }
        NodeState::Draining if !for_in_flight_work => {
            return EdgeSelection::NoMatch {
                reason: NoMatchReason::NodeDraining,
            };
        }
        NodeState::Active | NodeState::Degraded | NodeState::Draining => {}
    }

    // Operator pin: if set, only the pinned binding qualifies.
    if let OperatorPin::Pinned(pinned_id) = pin {
        if !local.supported.contains(&pinned_id) {
            return EdgeSelection::NoMatch {
                reason: NoMatchReason::PinnedBindingNotAdvertised,
            };
        }
        let advertised = target.bindings.iter().find(|ep| ep.binding_id == pinned_id);
        return match advertised {
            Some(ep) => EdgeSelection::Match(ep.clone()),
            None => EdgeSelection::NoMatch {
                reason: NoMatchReason::PinnedBindingNotAdvertised,
            },
        };
    }

    // Auto: pick the highest-ranked binding the client supports
    // AND the node advertises. Stable tie-break by binding-id (so
    // tests are deterministic when two bindings share the same
    // latency class — gRPC and TCP-framed both fall under Standard
    // / Low respectively, no current ties).
    let best = target
        .bindings
        .iter()
        .filter(|ep| local.supported.contains(&ep.binding_id))
        .max_by_key(|ep| (rank(ep.latency_class), ep.binding_id));

    match best {
        Some(ep) => EdgeSelection::Match(ep.clone()),
        None => EdgeSelection::NoMatch {
            reason: NoMatchReason::NoCompatibleBinding,
        },
    }
}

fn rank(c: LatencyClass) -> u8 {
    match c {
        LatencyClass::Rdma => 3,
        LatencyClass::Low => 2,
        LatencyClass::Standard => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::NodeId;
    use kiseki_proto::native_contract::{DrainState, ListenAddr};

    fn ep(binding_id: BindingId, latency_class: LatencyClass) -> BindingEndpoint {
        BindingEndpoint {
            binding_id,
            addr: ListenAddr::HostPort(format!("10.0.0.1:{:?}", binding_id)),
            latency_class,
            drain_state: None,
        }
    }

    fn ep_with_drain(binding_id: BindingId, latency_class: LatencyClass) -> BindingEndpoint {
        BindingEndpoint {
            binding_id,
            addr: ListenAddr::HostPort(format!("10.0.0.1:{:?}", binding_id)),
            latency_class,
            drain_state: Some(DrainState {
                quiesce_window_remaining_ms: 1000,
                accepts_new_work: false,
            }),
        }
    }

    fn active_node(bindings: Vec<BindingEndpoint>) -> NodeBindings {
        NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Active,
            bindings,
        }
    }

    fn local_supports(ids: &[BindingId]) -> LocalCapabilities {
        LocalCapabilities::from_iter(ids.iter().copied())
    }

    #[test]
    fn picks_highest_ranked_mutually_supported_binding() {
        let node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::TcpFramed, LatencyClass::Low),
        ]);
        let local = local_supports(&[BindingId::Grpc, BindingId::TcpFramed]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::TcpFramed);
            }
            other => panic!("expected Match, got: {other:?}"),
        }
    }

    #[test]
    fn picks_only_what_the_client_supports_locally() {
        // Node advertises both; client only has gRPC.
        let node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::TcpFramed, LatencyClass::Low),
        ]);
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::Grpc);
            }
            other => panic!("expected Match, got: {other:?}"),
        }
    }

    #[test]
    fn no_match_when_client_supports_nothing_node_advertises() {
        let node = active_node(vec![ep(BindingId::TcpFramed, LatencyClass::Low)]);
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::NoCompatibleBinding
            }
        );
    }

    #[test]
    fn rdma_outranks_low_outranks_standard() {
        // Node advertises three bindings of different classes; client
        // supports all three; selector picks Rdma.
        let node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::TcpFramed, LatencyClass::Low),
            ep(BindingId::Ibverbs, LatencyClass::Rdma),
        ]);
        let local = local_supports(&[
            BindingId::Grpc,
            BindingId::TcpFramed,
            BindingId::Ibverbs,
        ]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::Ibverbs);
                assert_eq!(picked.latency_class, LatencyClass::Rdma);
            }
            other => panic!("expected Rdma match, got: {other:?}"),
        }
    }

    #[test]
    fn pin_collapses_choice_to_pinned_binding() {
        let node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::TcpFramed, LatencyClass::Low),
        ]);
        let local = local_supports(&[BindingId::Grpc, BindingId::TcpFramed]);
        // Pinned to Grpc — must pick Grpc even though TcpFramed
        // would normally outrank.
        let outcome = select_for_edge(
            &node,
            &local,
            OperatorPin::Pinned(BindingId::Grpc),
            false,
        );
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::Grpc);
            }
            other => panic!("expected Grpc Match, got: {other:?}"),
        }
    }

    #[test]
    fn pin_to_unsupported_binding_returns_no_match() {
        let node = active_node(vec![ep(BindingId::Grpc, LatencyClass::Standard)]);
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(
            &node,
            &local,
            OperatorPin::Pinned(BindingId::Ibverbs),
            false,
        );
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::PinnedBindingNotAdvertised,
            }
        );
    }

    #[test]
    fn pin_to_binding_node_doesnt_advertise_returns_no_match() {
        let node = active_node(vec![ep(BindingId::Grpc, LatencyClass::Standard)]);
        let local = local_supports(&[BindingId::Grpc, BindingId::TcpFramed]);
        let outcome = select_for_edge(
            &node,
            &local,
            OperatorPin::Pinned(BindingId::TcpFramed),
            false,
        );
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::PinnedBindingNotAdvertised,
            }
        );
    }

    #[test]
    fn failed_node_returns_unreachable() {
        let node = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Failed,
            bindings: Vec::new(),
        };
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::NodeUnreachable,
            }
        );
    }

    #[test]
    fn evicted_node_returns_unreachable() {
        let node = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Evicted,
            bindings: Vec::new(),
        };
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::NodeUnreachable,
            }
        );
    }

    #[test]
    fn draining_node_skipped_for_new_work() {
        let node = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Draining,
            bindings: vec![ep_with_drain(BindingId::Grpc, LatencyClass::Standard)],
        };
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        assert_eq!(
            outcome,
            EdgeSelection::NoMatch {
                reason: NoMatchReason::NodeDraining,
            }
        );
    }

    #[test]
    fn draining_node_dialed_for_in_flight_work() {
        let node = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Draining,
            bindings: vec![ep_with_drain(BindingId::Grpc, LatencyClass::Standard)],
        };
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, true);
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::Grpc);
                assert!(picked.drain_state.is_some());
            }
            other => panic!("expected Match for in-flight work, got: {other:?}"),
        }
    }

    #[test]
    fn degraded_node_is_dialable() {
        let node = NodeBindings {
            node_id: NodeId(1),
            state: NodeState::Degraded,
            bindings: vec![ep(BindingId::Grpc, LatencyClass::Standard)],
        };
        let local = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(&node, &local, OperatorPin::Auto, false);
        match outcome {
            EdgeSelection::Match(_) => {}
            other => panic!("Degraded should be dialable, got: {other:?}"),
        }
    }

    /// Heterogeneous deployment scenario per §3.3: the same client
    /// session dials node A via libfabric/cxi and node B via
    /// TCP-framed. Demonstrates per-edge selection — the binding
    /// choice is independent across nodes.
    #[test]
    fn heterogeneous_cluster_yields_different_bindings_per_node() {
        let local = local_supports(&[
            BindingId::Grpc,
            BindingId::TcpFramed,
            BindingId::Ibverbs,
        ]);
        let slingshot_node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::Ibverbs, LatencyClass::Rdma),
        ]);
        let commodity_node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::TcpFramed, LatencyClass::Low),
        ]);

        let pick_a = select_for_edge(&slingshot_node, &local, OperatorPin::Auto, false);
        let pick_b = select_for_edge(&commodity_node, &local, OperatorPin::Auto, false);

        match (pick_a, pick_b) {
            (EdgeSelection::Match(a), EdgeSelection::Match(b)) => {
                assert_eq!(a.binding_id, BindingId::Ibverbs);
                assert_eq!(b.binding_id, BindingId::TcpFramed);
            }
            other => panic!("expected per-edge differentiation: {other:?}"),
        }
    }

    /// Cross-WAN client scenario per §3.3: a client without RDMA
    /// hardware reaching from outside the HPC fabric. The node
    /// advertises ibverbs but the client only has gRPC; the
    /// selector falls back to gRPC.
    #[test]
    fn cross_wan_client_falls_back_to_grpc() {
        let cluster_node = active_node(vec![
            ep(BindingId::Grpc, LatencyClass::Standard),
            ep(BindingId::Ibverbs, LatencyClass::Rdma),
        ]);
        let outside_client = local_supports(&[BindingId::Grpc]);
        let outcome = select_for_edge(
            &cluster_node,
            &outside_client,
            OperatorPin::Auto,
            false,
        );
        match outcome {
            EdgeSelection::Match(picked) => {
                assert_eq!(picked.binding_id, BindingId::Grpc);
            }
            other => panic!("expected gRPC fallback, got: {other:?}"),
        }
    }
}
