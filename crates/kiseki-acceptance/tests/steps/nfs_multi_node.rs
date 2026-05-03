//! Steps for `@integration @multi-node @nfs` scenarios.
//!
//! Wire-level NFSv4 against a real multi-node `kiseki-server` cluster.
//! The single-node `nfs_integration.rs` covers protocol compliance
//! against `world.server()`; this file covers the failure modes that
//! only emerge when the gateway routes through the cross-node fabric
//! (Phase 16+) — empty-file CREATE, CREATE+WRITE on EC 4+2, etc.

use cucumber::{then, when};

use crate::KisekiWorld;

fn cluster<'a>(w: &'a KisekiWorld) -> &'a crate::steps::cluster_harness::ClusterHarness {
    w.cluster
        .cluster_guard
        .as_deref()
        .expect("@multi-node @nfs step ran without `Given a 6-node kiseki cluster`")
}

fn nfs_addr_for_node(w: &KisekiWorld, node_id: u64) -> std::net::SocketAddr {
    let port = cluster(w).node(node_id).ports.nfs_tcp;
    format!("127.0.0.1:{port}").parse().expect("nfs addr parse")
}

#[when(regex = r"^a client opens-and-creates an empty file via NFSv4 on node-(\d+)$")]
async fn when_open_create_empty(w: &mut KisekiWorld, node_id: u64) {
    let addr = nfs_addr_for_node(w, node_id);
    let nfs = kiseki_client::remote_nfs::v4::Nfs4Client::v41(addr);
    // Empty parts → COMPOUND is PUTROOTFH + OPEN(CREATE) + COMMIT + GETFH.
    // The GCP 2026-05-02 failure: every such COMPOUND returned EIO on
    // the OPEN op (status=5) because `op_open` swallowed the inner
    // gateway error. We assert success here.
    match nfs.write_at_offsets(&[]).await {
        Ok(comp_id) => {
            w.last_composition_id = Some(comp_id);
            w.last_error = None;
        }
        Err(e) => {
            w.last_composition_id = None;
            w.last_error = Some(format!("{e}"));
        }
    }
}

#[when(regex = r#"^a client opens-creates-and-writes "([^"]*)" via NFSv4 on node-(\d+)$"#)]
async fn when_open_create_and_write(w: &mut KisekiWorld, payload: String, node_id: u64) {
    let addr = nfs_addr_for_node(w, node_id);
    let nfs = kiseki_client::remote_nfs::v4::Nfs4Client::v41(addr);
    let parts = vec![(0u64, payload.into_bytes())];
    match nfs.write_at_offsets(&parts).await {
        Ok(comp_id) => {
            w.last_composition_id = Some(comp_id);
            w.last_error = None;
        }
        Err(e) => {
            w.last_composition_id = None;
            w.last_error = Some(format!("{e}"));
        }
    }
}

#[then("the NFSv4 COMPOUND status is NFS4_OK")]
async fn then_compound_ok(w: &mut KisekiWorld) {
    assert!(
        w.last_error.is_none(),
        "NFSv4 COMPOUND failed: {:?}",
        w.last_error
    );
}

#[then("a composition id is returned in the GETFH reply")]
async fn then_composition_returned(w: &mut KisekiWorld) {
    assert!(
        w.last_composition_id.is_some(),
        "GETFH did not return a composition id"
    );
}

// ---------------------------------------------------------------------------
// pNFS LAYOUTGET wire scenario
// ---------------------------------------------------------------------------

/// PUT a 1KB unique-key object via S3 against node-1, then GET it
/// back from node-1 to make sure the leader's gateway can fully
/// read the composition (Raft Create + AppendChunkAndDelta both
/// committed, cluster_chunk_state seeded for fabric fan-out).
/// Without this, the subsequent pNFS DS-side read may transiently
/// fail with NFS4ERR_IO because its local gateway can't yet find
/// the chunk in cluster_chunk_state (the gateway's per-read 1 s
/// retry budget isn't enough on a heavily-exercised cluster
/// singleton). One single-node warmup is cheap and avoids the
/// cluster-wide N-node fan-out warmup.
#[when("a 1KB object is PUT via S3 to node-1")]
async fn when_1kb_put_to_node1(w: &mut KisekiWorld) {
    let key = format!("default/pnfs-{}", uuid::Uuid::new_v4().simple());
    let body = vec![0xabu8; 1024];
    let etag = {
        let guard = cluster(w);
        let node = guard.node(1);
        let url = format!("{}/{key}", node.s3_base);
        let resp = node
            .http
            .put(&url)
            .body(body.clone())
            .send()
            .await
            .expect("HTTP PUT failed");
        assert!(
            resp.status().is_success(),
            "S3 PUT to node-1 should succeed; got {}",
            resp.status(),
        );
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches('"').to_owned())
            .expect("S3 PUT response must carry ETag");

        // Single GET-by-uuid against the leader. Forces the full
        // read path (composition lookup + chunk fetch). Returns
        // when the leader can serve it, which means the local
        // chunk write completed AND cluster_chunk_state has the
        // entry — the seed for cross-node fabric reads.
        let get_url = format!("{}/default/{etag}", node.s3_base);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let resp = node
                .http
                .get(&get_url)
                .send()
                .await
                .expect("warmup GET failed");
            if resp.status().is_success() {
                let bytes = resp.bytes().await.unwrap_or_default();
                if bytes.as_ref() == body.as_slice() {
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                panic!("leader-warmup GET for composition {etag} did not succeed within 10s",);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        etag
    };
    w.cluster
        .name_index_state
        .insert("pnfs_comp_id".into(), etag);
}

/// Drive an NFSv4.1 LAYOUTGET COMPOUND against node-1's NFS port for
/// the composition stashed by the previous step. Then for each
/// returned mirror's deviceid issue a GETDEVICEINFO and collect the
/// netaddr pairs. Stash the (status, addrs) on the world for the
/// Then steps.
#[when("a client issues NFSv4.1 LAYOUTGET against node-1 for that composition")]
async fn when_layoutget_against_node1(w: &mut KisekiWorld) {
    use kiseki_client::remote_nfs::transport::RpcTransport;
    use kiseki_gateway::nfs4_server::op;
    use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};

    let comp_uuid_str = w
        .cluster
        .name_index_state
        .get("pnfs_comp_id")
        .cloned()
        .expect("previous PUT must stash pnfs_comp_id");
    let comp_uuid = uuid::Uuid::parse_str(&comp_uuid_str).expect("ETag is a UUID");

    let port = cluster(w).node(1).ports.nfs_tcp;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut transport = RpcTransport::connect(addr).expect("NFS connect");

    // EXCHANGE_ID — get a clientid.
    let mut body = XdrWriter::new();
    body.write_u32(0); // tag
    body.write_u32(1); // minor_version 1
    body.write_u32(1); // 1 op
    body.write_u32(op::EXCHANGE_ID);
    body.write_opaque_fixed(&[0u8; 8]); // verifier
    body.write_opaque(b"pnfs-bdd"); // owner
    body.write_u32(0); // flags
    body.write_u32(0); // SP4_NONE
    body.write_u32(0); // impl_id count
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .expect("EXCHANGE_ID call");
    let (client_id, _session_id) = parse_exchange_create(&reply, &mut transport, true);

    // CREATE_SESSION
    let mut body = XdrWriter::new();
    body.write_u32(0);
    body.write_u32(1);
    body.write_u32(1);
    body.write_u32(op::CREATE_SESSION);
    body.write_u64(client_id);
    body.write_u32(1); // sequence
    body.write_u32(0); // flags
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .expect("CREATE_SESSION call");
    let (_, session_id_opt) = parse_exchange_create(&reply, &mut transport, false);
    let session_id = session_id_opt.expect("CREATE_SESSION returns session_id");

    // COMPOUND: SEQUENCE + PUTROOTFH + LOOKUP("<uuid>") + LAYOUTGET
    let mut body = XdrWriter::new();
    body.write_u32(0); // tag
    body.write_u32(1); // minor_version
    body.write_u32(4); // 4 ops

    // SEQUENCE
    body.write_u32(op::SEQUENCE);
    body.write_opaque_fixed(&session_id);
    body.write_u32(2); // sequence_id (incremented from CREATE_SESSION)
    body.write_u32(0); // slot_id
    body.write_u32(0); // highest_slot_id
    body.write_u32(0); // sa_cachethis

    // PUTROOTFH
    body.write_u32(op::PUTROOTFH);

    // LOOKUP("<uuid>")
    body.write_u32(op::LOOKUP);
    body.write_string(&comp_uuid.to_string());

    // LAYOUTGET
    body.write_u32(op::LAYOUTGET);
    body.write_bool(false); // signal_layout_avail
    body.write_u32(4); // layout_type = LAYOUT4_FLEX_FILES
    body.write_u32(1); // iomode = LAYOUTIOMODE4_READ
    body.write_u64(0); // offset
    body.write_u64(u64::MAX); // length
    body.write_u64(0); // minlength
    body.write_opaque_fixed(&[0u8; 16]); // stateid
    body.write_u32(65_536); // maxcount

    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .expect("COMPOUND call");

    // Parse: COMPOUND status + tag + numops + (op + status)*4
    let mut r = XdrReader::new(&reply);
    let compound_status = r.read_u32().expect("status");
    w.cluster
        .name_index_state
        .insert("pnfs_compound_status".into(), compound_status.to_string());
    if compound_status != 0 {
        return;
    }
    let _tag = r.read_opaque().expect("tag");
    let _num = r.read_u32().expect("num");

    // Skip SEQUENCE op (op + status + session_id + sequenceid + slotid +
    //   highest_slotid + target_highest_slotid + status_flags)
    let _ = r.read_u32();
    let seq_st = r.read_u32().expect("seq status");
    if seq_st != 0 {
        w.cluster
            .name_index_state
            .insert("pnfs_compound_status".into(), seq_st.to_string());
        return;
    }
    let _ = r.read_opaque_fixed(16);
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let _ = r.read_u32();

    // PUTROOTFH (op + status)
    let _ = r.read_u32();
    let pr_st = r.read_u32().expect("putrootfh status");
    if pr_st != 0 {
        w.cluster
            .name_index_state
            .insert("pnfs_compound_status".into(), pr_st.to_string());
        return;
    }

    // LOOKUP (op + status)
    let _ = r.read_u32();
    let lk_st = r.read_u32().expect("lookup status");
    if lk_st != 0 {
        w.cluster
            .name_index_state
            .insert("pnfs_compound_status".into(), lk_st.to_string());
        return;
    }

    // LAYOUTGET: op + status + return_on_close + stateid + segments...
    let _ = r.read_u32();
    let lg_st = r.read_u32().expect("layoutget status");
    w.cluster
        .name_index_state
        .insert("pnfs_layoutget_status".into(), lg_st.to_string());
    if lg_st != 0 {
        return;
    }
    let _roc = r.read_bool();
    let _stateid = r.read_opaque_fixed(16);
    let n_segments = r.read_u32().expect("segments count") as usize;
    let mut device_ids: Vec<[u8; 16]> = Vec::new();
    for _ in 0..n_segments {
        let _ = r.read_u64(); // offset
        let _ = r.read_u64(); // length
        let _ = r.read_u32(); // iomode
        let _ = r.read_u32(); // layout_type
        let body = r.read_opaque().expect("layout body");
        // Parse FF body: stripe_unit (u64) + mirrors_count + per-mirror DS list
        let mut br = XdrReader::new(&body);
        let _stripe_unit = br.read_u64().expect("stripe_unit");
        let n_mirrors = br.read_u32().expect("mirrors") as usize;
        for _ in 0..n_mirrors {
            let n_ds = br.read_u32().expect("ds count") as usize;
            for _ in 0..n_ds {
                let did = br.read_opaque_fixed(16).expect("device_id");
                let mut buf = [0u8; 16];
                buf.copy_from_slice(&did);
                device_ids.push(buf);
                let _ = br.read_u32(); // efficiency
                let _ = br.read_opaque_fixed(16); // stateid
                let n_fh = br.read_u32().expect("fh count");
                let mut first_fh: Option<Vec<u8>> = None;
                for i in 0..n_fh {
                    let fh = br.read_opaque().expect("fh");
                    if i == 0 {
                        first_fh = Some(fh);
                    }
                }
                // Stash one (device_id, fh) pair per mirror for the
                // DS-read step. The first mirror is sufficient — every
                // mirror in a Replication-3 layout holds the same
                // bytes; in EC each mirror's fh points at its own
                // fragment but the DS-side READ uses the per-fragment
                // fh + offset and the kernel pNFS client picks one.
                if let Some(fh) = first_fh {
                    // Stash every mirror's (device_id, fh) so the
                    // DS-read step can fall back to other mirrors if
                    // one DS is momentarily busy. Mirrors past this
                    // first one have a stable insertion order in the
                    // FF layout body.
                    let key = format!("pnfs_mirror_{}_fh", device_ids.len() - 1);
                    w.cluster.name_index_state.insert(key, bytes_to_hex(&fh));
                    let key = format!("pnfs_mirror_{}_device_id", device_ids.len() - 1);
                    w.cluster.name_index_state.insert(key, bytes_to_hex(&buf));
                }
                let _ = br.read_opaque(); // user
                let _ = br.read_opaque(); // group
            }
        }
    }

    // Resolve every unique device_id via GETDEVICEINFO. We open a
    // fresh COMPOUND per device for simplicity (tests aren't perf-
    // sensitive). The aggregated uaddrs go on the world for the
    // Then step's assertion.
    let mut uaddrs: Vec<String> = Vec::new();
    let mut seq = 3;
    let mut seen: std::collections::HashSet<[u8; 16]> = std::collections::HashSet::new();
    for did in &device_ids {
        let did = *did;
        if !seen.insert(did) {
            continue;
        }
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(2); // SEQUENCE + GETDEVICEINFO
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&session_id);
        body.write_u32(seq);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::GETDEVICEINFO);
        body.write_opaque_fixed(&did);
        body.write_u32(4); // layout_type
        body.write_u32(65_536); // maxcount
        body.write_u32(0); // notify_types bitmap len
        seq += 1;

        let reply = transport
            .call(100_003, 4, 1, &body.into_bytes())
            .expect("GETDEVICEINFO call");
        let mut r = XdrReader::new(&reply);
        let st = r.read_u32().expect("status");
        if st != 0 {
            continue;
        }
        let _ = r.read_opaque(); // tag
        let _ = r.read_u32(); // num
        let _ = r.read_u32(); // SEQUENCE op
        let s_st = r.read_u32().expect("seq status");
        if s_st != 0 {
            continue;
        }
        let _ = r.read_opaque_fixed(16);
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32(); // GETDEVICEINFO op
        let g_st = r.read_u32().expect("gdi status");
        if g_st != 0 {
            continue;
        }
        let _layout_type = r.read_u32();
        let body = r.read_opaque().expect("device addr body");
        let mut br = XdrReader::new(&body);
        let n_addrs = br.read_u32().expect("netaddrs") as usize;
        // Per-device uaddrs go on the world for the assertion AND
        // the (device_id → first uaddr) mapping for the DS-read step.
        let mut first_for_this_device: Option<String> = None;
        for _ in 0..n_addrs {
            let _netid = br.read_string().expect("netid");
            let uaddr = br.read_string().expect("uaddr");
            if first_for_this_device.is_none() {
                first_for_this_device = Some(uaddr.clone());
            }
            uaddrs.push(uaddr);
        }
        if let Some(u) = first_for_this_device {
            // Find the mirror index whose device_id matches `did` and
            // stash this uaddr alongside that mirror's fh. The DS-read
            // step iterates over all mirrors in order, trying each one
            // until READ succeeds — kernel pNFS retry-on-IO behavior.
            let device_hex = bytes_to_hex(&did);
            let total_mirrors = device_ids.len();
            for i in 0..total_mirrors {
                let key = format!("pnfs_mirror_{i}_device_id");
                if w.cluster.name_index_state.get(&key) == Some(&device_hex) {
                    let uaddr_key = format!("pnfs_mirror_{i}_uaddr");
                    w.cluster.name_index_state.insert(uaddr_key, u.clone());
                    break;
                }
            }
        }
    }
    // Total mirror count for the DS-read step's iteration.
    w.cluster
        .name_index_state
        .insert("pnfs_mirror_count".into(), device_ids.len().to_string());
    w.cluster
        .name_index_state
        .insert("pnfs_uaddrs".into(), uaddrs.join("\n"));
}

/// Hex-encode arbitrary bytes for stashing in the cluster scratch
/// state. Avoids pulling the `hex` crate just for this.
fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Parse an NFSv4 `uaddr` (`a.b.c.d.PHI.PLO`) into a SocketAddr.
fn uaddr_to_socket(uaddr: &str) -> Option<std::net::SocketAddr> {
    let parts: Vec<&str> = uaddr.split('.').collect();
    if parts.len() != 6 {
        return None;
    }
    let ip = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3]);
    let hi: u16 = parts[4].parse().ok()?;
    let lo: u16 = parts[5].parse().ok()?;
    let port = (hi << 8) | lo;
    format!("{ip}:{port}").parse().ok()
}

/// Helper: parse a single-op COMPOUND reply that returns either
/// `client_id` (EXCHANGE_ID) or `session_id` (CREATE_SESSION).
/// Pure parsing — no further RPCs.
fn parse_exchange_create(
    reply: &[u8],
    _transport: &mut kiseki_client::remote_nfs::transport::RpcTransport,
    is_exchange: bool,
) -> (u64, Option<[u8; 16]>) {
    use kiseki_gateway::nfs_xdr::XdrReader;
    let mut r = XdrReader::new(reply);
    let _status = r.read_u32().expect("status");
    let _tag = r.read_opaque().expect("tag");
    let _num = r.read_u32().expect("num");
    let _op = r.read_u32().expect("op");
    let _op_status = r.read_u32().expect("op status");
    if is_exchange {
        let cid = r.read_u64().expect("client_id");
        (cid, None)
    } else {
        let sid_bytes = r.read_opaque_fixed(16).expect("session_id");
        let mut sid = [0u8; 16];
        sid.copy_from_slice(&sid_bytes);
        (0, Some(sid))
    }
}

#[then("the LAYOUTGET reply is NFS4_OK")]
async fn then_layoutget_ok(w: &mut KisekiWorld) {
    let compound: u32 = w
        .cluster
        .name_index_state
        .get("pnfs_compound_status")
        .and_then(|s| s.parse().ok())
        .expect("compound status not captured");
    assert_eq!(
        compound, 0,
        "COMPOUND status should be NFS4_OK; got {compound}"
    );
    let lg: u32 = w
        .cluster
        .name_index_state
        .get("pnfs_layoutget_status")
        .and_then(|s| s.parse().ok())
        .expect("LAYOUTGET status not captured");
    assert_eq!(lg, 0, "LAYOUTGET status should be NFS4_OK; got {lg}");
}

#[then(regex = r#"^the returned layout references all (\d+) node DS addresses$"#)]
async fn then_layout_references_ds_addrs(w: &mut KisekiWorld, want_count: usize) {
    // Build the expected uaddr-suffix set: each cluster node's ds_tcp
    // port encoded as "127.0.0.1.<port_hi>.<port_lo>".
    let want_uaddrs: std::collections::HashSet<String> = {
        let guard = cluster(w);
        guard
            .nodes()
            .map(|n| {
                let p = n.ports.ds_tcp;
                let hi = (p >> 8) & 0xff;
                let lo = p & 0xff;
                format!("127.0.0.1.{hi}.{lo}")
            })
            .collect()
    };
    let got_uaddrs: std::collections::HashSet<String> = w
        .cluster
        .name_index_state
        .get("pnfs_uaddrs")
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default();
    assert_eq!(
        got_uaddrs.len(),
        want_count,
        "expected {want_count} unique DS uaddrs in LAYOUTGET; got {} ({got_uaddrs:?})",
        got_uaddrs.len(),
    );
    for w_ua in &want_uaddrs {
        assert!(
            got_uaddrs.contains(w_ua),
            "node DS uaddr {w_ua:?} missing from LAYOUTGET reply (got {got_uaddrs:?})",
        );
    }
}

#[when("the client opens a session to a DS from the layout and reads the first stripe")]
async fn when_ds_read(w: &mut KisekiWorld) {
    use kiseki_client::remote_nfs::transport::RpcTransport;
    use kiseki_gateway::nfs4_server::op;
    use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};

    let mirror_count: usize = w
        .cluster
        .name_index_state
        .get("pnfs_mirror_count")
        .and_then(|s| s.parse().ok())
        .expect("LAYOUTGET step must stash pnfs_mirror_count");

    // Try each (uaddr, fh) pair in turn. Kernel pNFS does the same
    // when one DS returns IO-error on RC_RETRANS. Each individual
    // attempt also retries internally to absorb hydrator/fabric
    // timing on a freshly-PUT object under cluster load.
    let outer_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last_err = String::from("no mirrors tried");
    let data: Vec<u8> = 'outer: loop {
        for mirror_idx in 0..mirror_count {
            let uaddr_key = format!("pnfs_mirror_{mirror_idx}_uaddr");
            let fh_key = format!("pnfs_mirror_{mirror_idx}_fh");
            let Some(uaddr) = w.cluster.name_index_state.get(&uaddr_key).cloned() else {
                continue;
            };
            let Some(fh_hex) = w.cluster.name_index_state.get(&fh_key).cloned() else {
                continue;
            };
            let fh = hex_to_bytes(&fh_hex);
            let Some(addr) = uaddr_to_socket(&uaddr) else {
                continue;
            };
            match try_ds_read(addr, &fh).await {
                Ok(d) => break 'outer d,
                Err(e) => {
                    last_err = format!("mirror {mirror_idx} ({uaddr}): {e}");
                    if std::time::Instant::now() >= outer_deadline {
                        break;
                    }
                }
            }
        }
        if std::time::Instant::now() >= outer_deadline {
            panic!("DS read failed against every mirror within 30s — last: {last_err}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    w.cluster
        .name_index_state
        .insert("pnfs_ds_read_data".into(), bytes_to_hex(&data));
}

/// Run EXCHANGE_ID + CREATE_SESSION + (SEQUENCE+PUTFH+READ) against
/// a single DS endpoint and return the read bytes, or an error
/// string identifying the failure stage.
async fn try_ds_read(addr: std::net::SocketAddr, fh: &[u8]) -> Result<Vec<u8>, String> {
    use kiseki_client::remote_nfs::transport::RpcTransport;
    use kiseki_gateway::nfs4_server::op;
    use kiseki_gateway::nfs_xdr::{XdrReader, XdrWriter};

    let mut transport = RpcTransport::connect(addr).map_err(|e| format!("DS TCP connect: {e}"))?;

    // EXCHANGE_ID — DS distinguishes itself via ServerRole::Ds, but
    // the wire shape is identical.
    let mut body = XdrWriter::new();
    body.write_u32(0); // tag
    body.write_u32(1); // minor_version
    body.write_u32(1); // 1 op
    body.write_u32(op::EXCHANGE_ID);
    body.write_opaque_fixed(&[0u8; 8]);
    body.write_opaque(b"pnfs-ds-bdd");
    body.write_u32(0);
    body.write_u32(0);
    body.write_u32(0);
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .map_err(|e| format!("EXCHANGE_ID call: {e}"))?;
    let mut r = XdrReader::new(&reply);
    let _ = r.read_u32();
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let op_st = r
        .read_u32()
        .map_err(|e| format!("EXCHANGE_ID op_st: {e}"))?;
    if op_st != 0 {
        return Err(format!("EXCHANGE_ID returned {op_st}"));
    }
    let client_id = r.read_u64().map_err(|e| format!("client_id: {e}"))?;

    // CREATE_SESSION
    let mut body = XdrWriter::new();
    body.write_u32(0);
    body.write_u32(1);
    body.write_u32(1);
    body.write_u32(op::CREATE_SESSION);
    body.write_u64(client_id);
    body.write_u32(1);
    body.write_u32(0);
    let reply = transport
        .call(100_003, 4, 1, &body.into_bytes())
        .map_err(|e| format!("CREATE_SESSION call: {e}"))?;
    let mut r = XdrReader::new(&reply);
    let _ = r.read_u32();
    let _ = r.read_opaque();
    let _ = r.read_u32();
    let _ = r.read_u32();
    let cs_st = r.read_u32().map_err(|e| format!("cs_st: {e}"))?;
    if cs_st != 0 {
        return Err(format!("CREATE_SESSION returned {cs_st}"));
    }
    let session_id_bytes = r
        .read_opaque_fixed(16)
        .map_err(|e| format!("session_id: {e}"))?;
    let mut session_id = [0u8; 16];
    session_id.copy_from_slice(&session_id_bytes);

    // SEQUENCE + PUTFH(fh) + READ(offset=0, count=1024). Retry the
    // COMPOUND a few times to absorb transient hydrator/fabric
    // timing on this DS specifically — the outer loop in
    // `when_ds_read` cycles to a different DS if all retries fail.
    let mut sequence_id: u32 = 2;
    let inner_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let mut body = XdrWriter::new();
        body.write_u32(0);
        body.write_u32(1);
        body.write_u32(3);
        body.write_u32(op::SEQUENCE);
        body.write_opaque_fixed(&session_id);
        body.write_u32(sequence_id);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(0);
        body.write_u32(op::PUTFH);
        body.write_opaque(fh);
        body.write_u32(op::READ);
        body.write_opaque_fixed(&[0u8; 16]);
        body.write_u64(0);
        body.write_u32(1024);
        sequence_id += 1;

        let reply = transport
            .call(100_003, 4, 1, &body.into_bytes())
            .map_err(|e| format!("READ COMPOUND call: {e}"))?;
        let mut r = XdrReader::new(&reply);
        let compound_st = r.read_u32().map_err(|e| format!("compound_st: {e}"))?;
        let _ = r.read_opaque();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let seq_st = r.read_u32().map_err(|e| format!("seq_st: {e}"))?;
        if seq_st != 0 {
            return Err(format!("SEQUENCE returned {seq_st}"));
        }
        let _ = r.read_opaque_fixed(16);
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let _ = r.read_u32();
        let pf_st = r.read_u32().map_err(|e| format!("pf_st: {e}"))?;
        if pf_st != 0 {
            return Err(format!("PUTFH returned {pf_st} (compound={compound_st})"));
        }
        let _ = r.read_u32();
        let rd_st = r.read_u32().map_err(|e| format!("rd_st: {e}"))?;
        if rd_st == 0 {
            let _eof = r.read_bool();
            let data = r.read_opaque().map_err(|e| format!("read data: {e}"))?;
            return Ok(data);
        }
        if std::time::Instant::now() >= inner_deadline {
            return Err(format!("READ returned {rd_st} (compound={compound_st})"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[then("the bytes returned by the DS match the original PUT body")]
async fn then_ds_bytes_match(w: &mut KisekiWorld) {
    let actual_hex = w
        .cluster
        .name_index_state
        .get("pnfs_ds_read_data")
        .cloned()
        .expect("DS-read step must stash pnfs_ds_read_data");
    let actual = hex_to_bytes(&actual_hex);
    let expected = vec![0xabu8; 1024];
    assert_eq!(
        actual.len(),
        expected.len(),
        "DS read returned wrong length"
    );
    assert_eq!(actual, expected, "DS read returned wrong bytes");
}
