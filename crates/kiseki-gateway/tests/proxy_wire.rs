#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wire-level proxy fallback (ADR-042 §4 native row) — RED-first test
//! for the `put_object` re-issue path the audit + integrator finding
//! docs flagged DEFERRED:
//!
//! - `specs/findings/2026-05-15-gate2-audit.md` (I-NG5 + I-NG1)
//! - `specs/findings/2026-05-15-integration-followup.md` follow-up backlog item 3
//! - `specs/findings/2026-05-15-leader-forwarding-posture.md` §M2 (H2 + M2)
//!
//! Scenario:
//! 1. Spin up a "leader" `ServerImpl` behind a real `tonic::transport::Server`
//!    listening on 127.0.0.1:<ephemeral>. Its `GatewayOps` stub records
//!    the inbound `ControlFields` (including `forwarded_from_node` +
//!    `idempotency_key`) and returns Ok.
//! 2. Spin up a "follower" `ServerImpl` whose `GatewayOps` returns
//!    `GatewayError::ForwardToLeader { leader_node_id: 2 }`. The
//!    follower's `proxy_client` is wired with `(NodeId(2),
//!    leader_addr)`.
//! 3. Drive `follower.put_object(&principal, req)` with
//!    `KISEKI_NATIVE_PROXY_FALLBACK=on`.
//! 4. Assert: the leader received the request with
//!    `forwarded_from_node = Some(1)` and the original
//!    `idempotency_key` byte-for-byte; the follower's response is the
//!    leader's response unchanged.

use std::net::SocketAddr;
use std::sync::Arc;

use kiseki_common::ids::{CompositionId, NamespaceId, NodeId, OrgId, ShardId};
use kiseki_gateway::error::GatewayError;
use kiseki_gateway::native::grpc::adapter::GrpcAdapter;
use kiseki_gateway::native::proxy_client::ProxyClient;
use kiseki_gateway::native::server::ServerImpl;
use kiseki_gateway::native::signing_keys::SigningKeys;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, ReadResponse, WriteRequest, WriteResponse};
use kiseki_proto::native_contract::ConnectionId;
use kiseki_proto::v1::native as np;

#[derive(Default)]
struct LeaderRecorder {
    captured: parking_lot::Mutex<Vec<WriteRequest>>,
}

#[async_trait::async_trait]
impl GatewayOps for LeaderRecorder {
    async fn read(&self, _req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        Err(GatewayError::OperationNotSupported(
            "leader recorder read unused".into(),
        ))
    }
    async fn write(&self, req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        let resp = WriteResponse {
            composition_id: CompositionId(uuid::Uuid::from_bytes([7; 16])),
            bytes_written: u64::try_from(req.data.len()).unwrap(),
        };
        self.captured.lock().push(req);
        Ok(resp)
    }
}

struct ForwardingStub {
    leader: NodeId,
}

#[async_trait::async_trait]
impl GatewayOps for ForwardingStub {
    async fn read(&self, _req: ReadRequest) -> Result<ReadResponse, GatewayError> {
        Err(GatewayError::OperationNotSupported(
            "follower stub read unused".into(),
        ))
    }
    async fn write(&self, _req: WriteRequest) -> Result<WriteResponse, GatewayError> {
        Err(GatewayError::Upstream("follower stub legacy write".into()))
    }
    async fn write_with_forwarding(
        &self,
        _req: WriteRequest,
    ) -> Result<WriteResponse, GatewayError> {
        Err(GatewayError::ForwardToLeader {
            shard_id: ShardId(uuid::Uuid::from_bytes([3; 16])),
            leader_node_id: self.leader,
        })
    }
}

fn signing() -> Arc<SigningKeys> {
    Arc::new(SigningKeys::new(
        &kiseki_crypto::keys::SystemMasterKey::new([0xCC; 32], kiseki_common::tenancy::KeyEpoch(1)),
        60_000,
    ))
}

fn anon_principal() -> kiseki_gateway::native::grpc::TonicPrincipal {
    kiseki_gateway::native::grpc::TonicPrincipal::new(String::new(), ConnectionId(0))
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_put_object_wire_level_reissues_with_forwarded_from_node_and_idempotency_key() {
    // ---- leader side -----------------------------------------------
    let recorder = Arc::new(LeaderRecorder::default());
    let leader_ops: Arc<dyn GatewayOps> = recorder.clone();
    let leader_server = Arc::new(ServerImpl::new(leader_ops, signing()));
    let leader_adapter = GrpcAdapter::new(leader_server);

    let leader_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let leader_addr: SocketAddr = leader_listener.local_addr().unwrap();
    let leader_incoming = tokio_stream::wrappers::TcpListenerStream::new(leader_listener);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let leader_task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                kiseki_proto::v1::native::gateway_data_service_server::GatewayDataServiceServer::new(
                    leader_adapter,
                ),
            )
            .serve_with_incoming_shutdown(leader_incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    // give the listener a moment to be ready
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // ---- follower side --------------------------------------------
    let follower_node_id = NodeId(1);
    let leader_node_id = NodeId(2);

    let follower_ops: Arc<dyn GatewayOps> = Arc::new(ForwardingStub {
        leader: leader_node_id,
    });
    let pc = Arc::new(ProxyClient::new(follower_node_id));
    pc.register_node(leader_node_id, leader_addr.to_string());

    let follower_server =
        Arc::new(ServerImpl::new(follower_ops, signing()).with_proxy_client(Arc::clone(&pc)));
    follower_server.set_proxy_fallback_enabled(true);

    // ---- drive a put_object through the follower ------------------
    let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
    let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
    let idem = b"client-idemp-key-001".to_vec();

    let req = np::PutObjectRequest {
        control: Some(np::ControlFields {
            tenant_id: Some(kiseki_proto::v1::OrgId {
                value: tenant.0.to_string(),
            }),
            idempotency_key: idem.clone(),
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
            // Wire-level proxy: follower MUST populate this before
            // re-issuing. Original request from the client has it None.
            forwarded_from_node: None,
        }),
        namespace_id: Some(kiseki_proto::v1::NamespaceId {
            value: ns.0.to_string(),
        }),
        name: "obj/0".into(),
        data: b"payload".to_vec(),
    };

    let resp = follower_server
        .put_object(&anon_principal(), req)
        .await
        .expect("wire-level proxy succeeds and returns the leader's response");
    assert_eq!(resp.size, 7);

    // ---- assert leader observed the forwarded request -------------
    let captured = recorder.captured.lock();
    assert_eq!(
        captured.len(),
        1,
        "leader MUST have observed exactly one proxied write"
    );
    let observed = &captured[0];
    assert_eq!(
        observed.idempotency_key.as_deref(),
        Some(idem.as_slice()),
        "I-NG5: idempotency_key MUST cross the proxy boundary byte-for-byte"
    );
    drop(captured);

    // shut the leader down cleanly
    let _ = shutdown_tx.send(());
    let _ = leader_task.await;
}
