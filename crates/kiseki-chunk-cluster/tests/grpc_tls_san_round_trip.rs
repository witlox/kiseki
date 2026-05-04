#![allow(clippy::unwrap_used, clippy::expect_used)]
//! mTLS + SAN-role end-to-end smoke test (Phase 16a step 12).
//!
//! Spins up a `ClusterChunkServer` over real TLS with the SAN
//! interceptor wired in, then a `GrpcFabricPeer` connecting with a
//! fabric-role client cert. Tests both happy-path (round trip works)
//! and the security guarantee (a client without the fabric SAN is
//! rejected at the interceptor).
//!
//! Cert generation uses `rcgen` (existing dev-dep). For the test rig
//! we issue:
//!
//! - A self-signed CA.
//! - A server cert with DNS SAN `localhost` + fabric SPIFFE URI.
//! - A *fabric* client cert with the fabric SPIFFE URI.
//! - A *tenant* client cert with a tenant SPIFFE URI (no fabric).
//!
//! The interceptor must accept the first and reject the second.

use std::sync::Arc;
use std::time::Duration;

use kiseki_chunk::pool::{AffinityPool, DeviceClass, DurabilityStrategy};
use kiseki_chunk::store::ChunkStore;
use kiseki_chunk::{AsyncChunkOps, SyncBridge};
use kiseki_chunk_cluster::peer::FabricPeerError;
use kiseki_chunk_cluster::{
    fabric_san_interceptor, ClusterChunkServer, FabricPeer, GrpcFabricPeer,
};
use kiseki_common::ids::{ChunkId, OrgId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::envelope::Envelope;
use kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkServiceServer;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, SanType};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};

#[allow(clippy::struct_field_names)]
struct TlsBundle {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    fabric_client_cert_pem: String,
    fabric_client_key_pem: String,
    tenant_client_cert_pem: String,
    tenant_client_key_pem: String,
}

fn issue_test_certs() -> TlsBundle {
    // 1. CA.
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::new(vec![]).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-test-ca");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca self sign");
    let ca_pem = ca_cert.pem();
    let ca_issuer = rcgen::Issuer::new(ca_params, ca_key);

    // 2. Server cert: localhost + fabric SPIFFE URI.
    let server_key = KeyPair::generate().expect("server key");
    let mut server_params =
        CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
    server_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-fabric-server");
    server_params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::URI("spiffe://cluster/fabric/test-server".try_into().unwrap()),
    ];
    let server_cert = server_params
        .signed_by(&server_key, &ca_issuer)
        .expect("server signed");
    let server_cert_pem = server_cert.pem();
    let server_key_pem = server_key.serialize_pem();

    // 3. Fabric client cert.
    let fabric_key = KeyPair::generate().expect("fabric client key");
    let mut fabric_params = CertificateParams::new(vec![]).expect("fabric params");
    fabric_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-fabric-client");
    fabric_params.subject_alt_names = vec![SanType::URI(
        "spiffe://cluster/fabric/test-client".try_into().unwrap(),
    )];
    let fabric_cert = fabric_params
        .signed_by(&fabric_key, &ca_issuer)
        .expect("fabric signed");
    let fabric_client_cert_pem = fabric_cert.pem();
    let fabric_client_key_pem = fabric_key.serialize_pem();

    // 4. Tenant client cert (NOT fabric — must be rejected).
    let tenant_key = KeyPair::generate().expect("tenant key");
    let mut tenant_params = CertificateParams::new(vec![]).expect("tenant params");
    tenant_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kiseki-tenant-client");
    tenant_params.subject_alt_names = vec![SanType::URI(
        "spiffe://cluster/org/00000000-0000-0000-0000-000000000001"
            .try_into()
            .unwrap(),
    )];
    let tenant_cert = tenant_params
        .signed_by(&tenant_key, &ca_issuer)
        .expect("tenant signed");
    let tenant_client_cert_pem = tenant_cert.pem();
    let tenant_client_key_pem = tenant_key.serialize_pem();

    TlsBundle {
        ca_pem,
        server_cert_pem,
        server_key_pem,
        fabric_client_cert_pem,
        fabric_client_key_pem,
        tenant_client_cert_pem,
        tenant_client_key_pem,
    }
}

fn local_bridge(pool: &str) -> Arc<dyn AsyncChunkOps> {
    let mut store = ChunkStore::new();
    store.add_pool(AffinityPool {
        name: pool.to_owned(),
        device_class: DeviceClass::NvmeSsd,
        durability: DurabilityStrategy::Replication { copies: 1 },
        devices: vec![],
        capacity_bytes: 1 << 30,
        used_bytes: 0,
    });
    Arc::new(SyncBridge::new(store))
}

fn make_envelope(seed: u8) -> Envelope {
    Envelope {
        chunk_id: ChunkId([seed; 32]),
        ciphertext: vec![seed; 64],
        auth_tag: [0u8; 16],
        nonce: [0u8; 12],
        system_epoch: KeyEpoch(1),
        tenant_epoch: None,
        tenant_wrapped_material: None,
    }
}

/// Spin up a TLS-fronted `ClusterChunkServer` with the SAN interceptor
/// wired in. Returns (local store, bound addr, TLS bundle).
async fn start_tls_server(pool: &str) -> (Arc<dyn AsyncChunkOps>, std::net::SocketAddr, TlsBundle) {
    let bundle = issue_test_certs();
    let local = local_bridge(pool);
    let server = ClusterChunkServer::new(Arc::clone(&local), pool);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stream = TcpListenerStream::new(listener);

    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            bundle.server_cert_pem.as_bytes(),
            bundle.server_key_pem.as_bytes(),
        ))
        .client_ca_root(Certificate::from_pem(bundle.ca_pem.as_bytes()));

    let svc = ClusterChunkServiceServer::with_interceptor(server, fabric_san_interceptor);
    tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("server tls config")
            .add_service(svc)
            .serve_with_incoming(stream)
            .await
            .expect("server");
    });
    (local, addr, bundle)
}

async fn build_tls_channel(
    addr: std::net::SocketAddr,
    ca_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> tonic::transport::Channel {
    let uri: tonic::transport::Uri = format!("https://localhost:{}", addr.port())
        .parse()
        .expect("uri");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem.as_bytes()))
        .identity(Identity::from_pem(
            client_cert_pem.as_bytes(),
            client_key_pem.as_bytes(),
        ))
        .domain_name("localhost");

    // Eager retry until the server is listening (small race window).
    loop {
        let endpoint = Endpoint::from(uri.clone())
            .timeout(Duration::from_secs(5))
            .tls_config(tls.clone())
            .expect("client tls config");
        if let Ok(channel) = endpoint.connect().await {
            return channel;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn fabric_san_client_round_trips_through_mtls() {
    let (_local, addr, bundle) = start_tls_server("p").await;
    let channel = build_tls_channel(
        addr,
        &bundle.ca_pem,
        &bundle.fabric_client_cert_pem,
        &bundle.fabric_client_key_pem,
    )
    .await;
    let peer = GrpcFabricPeer::new("fabric-test", channel);

    let env = make_envelope(0x42);
    let chunk_id = env.chunk_id;
    let tenant = OrgId(uuid::Uuid::nil());

    let stored = peer
        .put_fragment(chunk_id, 0, tenant, "p".into(), env.clone())
        .await
        .expect("put through mTLS+SAN");
    assert!(stored, "fragment newly stored over mTLS");

    let got = peer
        .get_fragment(chunk_id, 0)
        .await
        .expect("get through mTLS");
    assert_eq!(got.ciphertext, env.ciphertext);
}

#[tokio::test]
async fn tenant_san_client_rejected_at_interceptor() {
    let (_local, addr, bundle) = start_tls_server("p").await;
    let channel = build_tls_channel(
        addr,
        &bundle.ca_pem,
        &bundle.tenant_client_cert_pem,
        &bundle.tenant_client_key_pem,
    )
    .await;
    let peer = GrpcFabricPeer::new("tenant-impostor", channel);

    let env = make_envelope(0x55);
    let tenant = OrgId(uuid::Uuid::nil());
    let err = peer
        .put_fragment(env.chunk_id, 0, tenant, "p".into(), env.clone())
        .await
        .expect_err("tenant cert MUST be rejected");
    assert!(
        matches!(err, FabricPeerError::Rejected(_)),
        "expected Rejected (PermissionDenied), got {err:?}"
    );
}
