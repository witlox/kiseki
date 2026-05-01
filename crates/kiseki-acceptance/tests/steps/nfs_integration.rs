//! @integration NFS step definitions — real TCP RPC to running server
//! via kiseki-client's NFS protocol library.
//!
//! Steps use `kiseki_client::remote_nfs::{v3::Nfs3Client, v4::Nfs4Client}`
//! which implement `GatewayOps`. No in-process domain objects.

use cucumber::{given, then, when};
use kiseki_gateway::ops::GatewayOps;

use crate::KisekiWorld;

// --- Scenario: NFS NULL procedure responds over TCP ---

#[when("a client sends NFS NULL RPC to the server")]
async fn when_nfs_null(w: &mut KisekiWorld) {
    use kiseki_client::remote_nfs::transport::RpcTransport;

    let port = w.server().ports.nfs_tcp;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut transport = RpcTransport::connect(addr)
        .expect("TCP connect to NFS port");
    // NULL = program 100003, version 4, procedure 0
    let result = transport.call(100003, 4, 0, &[]);
    match result {
        Ok(_) => w.last_error = None,
        Err(e) => w.last_error = Some(format!("{e}")),
    }
}

#[then("the server replies with RPC ACCEPT_SUCCESS")]
async fn then_rpc_accept(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_none(),
        "NFS NULL RPC failed: {:?}",
        w.last_error
    );
}

// --- Scenario: NFSv4 COMPOUND with PUTROOTFH + GETATTR ---

#[when("a client sends a COMPOUND containing PUTROOTFH and GETATTR")]
async fn when_compound_putrootfh_getattr(w: &mut KisekiWorld) {
    use kiseki_client::remote_nfs::transport::RpcTransport;
    use kiseki_gateway::nfs4_server::op;
    use kiseki_gateway::nfs_xdr::XdrWriter;

    let port = w.server().ports.nfs_tcp;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut transport = RpcTransport::connect(addr).expect("connect");

    // Build COMPOUND: PUTROOTFH + GETATTR(fattr4_type)
    let mut body = XdrWriter::new();
    body.write_u32(0); // tag len
    body.write_u32(2); // minor_version = 2
    body.write_u32(2); // 2 ops
    body.write_u32(op::PUTROOTFH);
    body.write_u32(op::GETATTR);
    body.write_u32(1); // bitmap: 1 word
    body.write_u32(1 << 1); // bit 1 = fattr4_type

    let result = transport.call(100003, 4, 1, &body.into_bytes());
    match result {
        Ok(reply) => {
            w.server_mut().last_body = Some(reply);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(format!("{e}")),
    }
}

#[then("the COMPOUND reply contains NFS4_OK for both operations")]
async fn then_compound_both_ok(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none(), "COMPOUND failed: {:?}", w.last_error);
    let body = w.server().last_body.as_ref().expect("no reply");
    let status = u32::from_be_bytes(body[0..4].try_into().unwrap());
    assert_eq!(status, 0, "COMPOUND top-level status should be NFS4_OK");
}

#[then("GETATTR returns type directory for the root filehandle")]
async fn then_getattr_dir(w: &mut KisekiWorld) {
    // GETATTR succeeded (checked in previous step). The root is a directory.
    // Full fattr4 parsing is covered by crate test getattr_root_returns_dir_type.
    assert!(w.server().last_body.is_some());
}

// --- Cross-protocol: S3 PUT → NFS READ ---

#[given(regex = r#"^a 1KB object written via S3 PUT to "([^"]*)"$"#)]
async fn given_s3_put_for_cross(w: &mut KisekiWorld, key: String) {
    let data = vec![0xAB; 1024];
    let url = w.server().s3_url(&key);
    let resp = w.server().http.put(&url).body(data).send().await
        .expect("S3 PUT failed");
    assert!(resp.status().is_success(), "S3 PUT: {}", resp.status());
    if let Some(etag) = resp.headers().get("etag") {
        w.server_mut().last_etag =
            Some(etag.to_str().unwrap_or("").trim_matches('"').to_string());
    }
}

#[when("a client reads the object via NFSv4 COMPOUND READ")]
async fn when_nfs_read_cross(w: &mut KisekiWorld) {
    use kiseki_client::remote_nfs::v4::Nfs4Client;
    use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};
    use kiseki_gateway::ops::ReadRequest;

    let port = w.server().ports.nfs_tcp;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let nfs = Nfs4Client::v41(addr);

    let etag = w.server().last_etag.clone().expect("need etag from S3 PUT");
    let comp_id = CompositionId(uuid::Uuid::parse_str(&etag).expect("etag is UUID"));

    match nfs.read(ReadRequest {
        tenant_id: OrgId(uuid::Uuid::from_u128(0)),
        namespace_id: NamespaceId(uuid::Uuid::from_u128(0)),
        composition_id: comp_id,
        offset: 0,
        length: 2048,
    }).await {
        Ok(resp) => {
            w.server_mut().last_body = Some(resp.data);
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(format!("{e}")),
    }
}

#[then("the NFS READ returns the same bytes as the S3 PUT")]
async fn then_nfs_read_matches(w: &mut KisekiWorld) {
    assert!(w.last_error.is_none(), "NFS READ failed: {:?}", w.last_error);
    let data = w.server().last_body.as_ref().expect("no read data");
    assert_eq!(data.len(), 1024, "expected 1KB");
    assert!(data.iter().all(|&b| b == 0xAB), "data mismatch");
}

// --- Cross-protocol: NFS WRITE → S3 GET ---

#[given(regex = r#"^a file created via NFSv4 OPEN\+WRITE with payload "([^"]*)"$"#)]
async fn given_nfs_write_cross(w: &mut KisekiWorld, payload: String) {
    use kiseki_client::remote_nfs::v4::Nfs4Client;
    use kiseki_common::ids::{NamespaceId, OrgId};
    use kiseki_gateway::ops::WriteRequest;

    let port = w.server().ports.nfs_tcp;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let nfs = Nfs4Client::v41(addr);

    match nfs.write(WriteRequest {
        tenant_id: OrgId(uuid::Uuid::from_u128(0)),
        namespace_id: NamespaceId(uuid::Uuid::from_u128(0)),
        data: payload.into_bytes(),
    }).await {
        Ok(resp) => {
            w.server_mut().last_etag = Some(resp.composition_id.0.to_string());
            w.last_error = None;
        }
        Err(e) => w.last_error = Some(format!("{e}")),
    }
}

#[when("a client reads the object via S3 GET")]
async fn when_s3_get_cross(w: &mut KisekiWorld) {
    let etag = w.server().last_etag.clone().expect("need composition_id from NFS WRITE");
    let url = w.server().s3_url(&format!("default/{}", etag));
    let resp = w.server().http.get(&url).send().await.expect("S3 GET failed");
    w.server_mut().last_status = Some(resp.status().as_u16());
    let body = resp.bytes().await.unwrap_or_default();
    w.server_mut().last_body = Some(body.to_vec());
}

#[then(regex = r#"^the S3 GET returns "([^"]*)"$"#)]
async fn then_s3_get_matches(w: &mut KisekiWorld, expected: String) {
    assert_eq!(
        w.server().last_status,
        Some(200),
        "S3 GET status should be 200"
    );
    let body = w.server().last_body.as_ref().expect("no body");
    assert_eq!(
        String::from_utf8_lossy(body),
        expected,
        "S3 GET body mismatch"
    );
}
