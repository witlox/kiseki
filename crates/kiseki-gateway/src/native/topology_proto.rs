//! Convert from kiseki-transport's `SelectorPlan` (and the contract-
//! layer types it uses) into the proto wire shape ADR-042 §1.7
//! ships in `TopologyInfo.NodeInfo`. Centralized here so both the
//! runtime startup path and any future control-plane gossip paths
//! produce identical proto output.

use kiseki_proto::native_contract as nc;
use kiseki_proto::v1::native as np;
use kiseki_transport::native::{AvailableBinding, SelectorPlan};

/// Build a single `NodeInfo` advertising the bindings the local
/// node is spawning. ADR-042 §1.7 + §16.1 phase 5: this is what
/// remote clients consume via `GetTopology` to drive per-edge
/// selection (kiseki-client::native::edge_selector).
///
/// `data_addr` retains the legacy single-address shape for v0
/// clients — populated with the gRPC binding's address when present,
/// otherwise the first available binding's `host:port`.
#[must_use]
pub fn node_info_from_plan(
    node_id: u64,
    state: np::NodeState,
    plan: &SelectorPlan,
) -> np::NodeInfo {
    let bindings: Vec<np::BindingEndpoint> = plan
        .spawn_order
        .iter()
        .map(binding_endpoint_to_proto)
        .collect();
    let legacy_addr = bindings
        .iter()
        .find(|b| b.binding_id == np::BindingId::Grpc as i32)
        .or_else(|| bindings.first())
        .map(|b| b.addr.clone())
        .unwrap_or_default();
    np::NodeInfo {
        node_id,
        data_addr: legacy_addr,
        state: state as i32,
        bindings,
    }
}

fn binding_endpoint_to_proto(ab: &AvailableBinding) -> np::BindingEndpoint {
    np::BindingEndpoint {
        binding_id: binding_id_to_proto(ab.binding_id) as i32,
        addr: listen_addr_string(&ab.addr),
        latency_class: latency_class_to_proto(ab.latency_class) as i32,
        // Drain state populated only when the node's own state
        // transitions to DRAINING — runtime owns that lifecycle.
        // SelectorPlan reflects "what we're spawning"; it doesn't
        // know about drain. ADR-042 §1.7.1.
        drain_state: None,
    }
}

fn binding_id_to_proto(id: nc::BindingId) -> np::BindingId {
    match id {
        nc::BindingId::Grpc => np::BindingId::Grpc,
        nc::BindingId::TcpFramed => np::BindingId::TcpFramed,
        nc::BindingId::Ibverbs => np::BindingId::Ibverbs,
        nc::BindingId::Libfabric { .. } => np::BindingId::Libfabric,
    }
}

fn latency_class_to_proto(c: nc::LatencyClass) -> np::LatencyClass {
    match c {
        nc::LatencyClass::Standard => np::LatencyClass::Standard,
        nc::LatencyClass::Low => np::LatencyClass::Low,
        nc::LatencyClass::Rdma => np::LatencyClass::Rdma,
    }
}

fn listen_addr_string(addr: &nc::ListenAddr) -> String {
    match addr {
        nc::ListenAddr::HostPort(s) => s.clone(),
        // Fabric descriptors aren't IP addresses; surface as a
        // tagged hex string so v0 clients see something concrete
        // rather than empty. v1 clients consult the contract type
        // via the binding-aware path, not this string.
        nc::ListenAddr::FabricDescriptor(bytes) => {
            use std::fmt::Write as _;
            let mut s = String::with_capacity(7 + bytes.len() * 2);
            s.push_str("fabric:");
            for b in bytes {
                let _ = write!(s, "{b:02x}");
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_proto::native_contract as nc;
    use kiseki_transport::native::{AvailableBinding, OperatorPin};

    fn plan_with(bindings: Vec<AvailableBinding>) -> SelectorPlan {
        SelectorPlan {
            spawn_order: bindings,
            pin: OperatorPin::Auto,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn node_info_carries_every_binding_in_spawn_order() {
        let plan = plan_with(vec![
            AvailableBinding {
                binding_id: nc::BindingId::TcpFramed,
                latency_class: nc::LatencyClass::Low,
                addr: nc::ListenAddr::HostPort("10.0.0.1:9101".into()),
            },
            AvailableBinding {
                binding_id: nc::BindingId::Grpc,
                latency_class: nc::LatencyClass::Standard,
                addr: nc::ListenAddr::HostPort("10.0.0.1:9100".into()),
            },
        ]);
        let info = node_info_from_plan(7, np::NodeState::Active, &plan);
        assert_eq!(info.node_id, 7);
        assert_eq!(info.bindings.len(), 2);
        assert_eq!(info.bindings[0].binding_id, np::BindingId::TcpFramed as i32);
        assert_eq!(info.bindings[0].addr, "10.0.0.1:9101");
        assert_eq!(info.bindings[1].binding_id, np::BindingId::Grpc as i32);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_data_addr_prefers_grpc_for_v0_clients() {
        let plan = plan_with(vec![
            AvailableBinding {
                binding_id: nc::BindingId::TcpFramed,
                latency_class: nc::LatencyClass::Low,
                addr: nc::ListenAddr::HostPort("10.0.0.1:9101".into()),
            },
            AvailableBinding {
                binding_id: nc::BindingId::Grpc,
                latency_class: nc::LatencyClass::Standard,
                addr: nc::ListenAddr::HostPort("10.0.0.1:9100".into()),
            },
        ]);
        let info = node_info_from_plan(1, np::NodeState::Active, &plan);
        assert_eq!(
            info.data_addr, "10.0.0.1:9100",
            "legacy data_addr must point at the gRPC binding for v0-client compat",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_data_addr_falls_back_to_first_when_grpc_absent() {
        // TCP-framed-only deployment (operator pinned to TCP-framed).
        let plan = plan_with(vec![AvailableBinding {
            binding_id: nc::BindingId::TcpFramed,
            latency_class: nc::LatencyClass::Low,
            addr: nc::ListenAddr::HostPort("10.0.0.1:9101".into()),
        }]);
        let info = node_info_from_plan(1, np::NodeState::Active, &plan);
        assert_eq!(info.data_addr, "10.0.0.1:9101");
        assert_eq!(info.bindings.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_plan_yields_empty_bindings_and_empty_data_addr() {
        let plan = plan_with(Vec::new());
        let info = node_info_from_plan(1, np::NodeState::Active, &plan);
        assert!(info.bindings.is_empty());
        assert!(info.data_addr.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn node_state_threads_through_to_proto() {
        let plan = plan_with(Vec::new());
        for (state, want) in [
            (np::NodeState::Active, np::NodeState::Active as i32),
            (np::NodeState::Degraded, np::NodeState::Degraded as i32),
            (np::NodeState::Draining, np::NodeState::Draining as i32),
            (np::NodeState::Failed, np::NodeState::Failed as i32),
            (np::NodeState::Evicted, np::NodeState::Evicted as i32),
        ] {
            let info = node_info_from_plan(1, state, &plan);
            assert_eq!(info.state, want, "state {state:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn libfabric_binding_id_collapses_to_proto_libfabric() {
        let plan = plan_with(vec![AvailableBinding {
            binding_id: nc::BindingId::Libfabric {
                provider: nc::LibfabricProvider::Cxi,
            },
            latency_class: nc::LatencyClass::Rdma,
            addr: nc::ListenAddr::FabricDescriptor(vec![0xCA, 0xFE]),
        }]);
        let info = node_info_from_plan(1, np::NodeState::Active, &plan);
        assert_eq!(info.bindings[0].binding_id, np::BindingId::Libfabric as i32);
        assert!(
            info.bindings[0].addr.starts_with("fabric:"),
            "fabric descriptor surfaces as tagged hex, got: {}",
            info.bindings[0].addr,
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latency_class_mapping_is_total() {
        for (nc_class, proto_class) in [
            (nc::LatencyClass::Standard, np::LatencyClass::Standard),
            (nc::LatencyClass::Low, np::LatencyClass::Low),
            (nc::LatencyClass::Rdma, np::LatencyClass::Rdma),
        ] {
            let plan = plan_with(vec![AvailableBinding {
                binding_id: nc::BindingId::Grpc,
                latency_class: nc_class,
                addr: nc::ListenAddr::HostPort("10.0.0.1:9100".into()),
            }]);
            let info = node_info_from_plan(1, np::NodeState::Active, &plan);
            assert_eq!(info.bindings[0].latency_class, proto_class as i32);
        }
    }
}
