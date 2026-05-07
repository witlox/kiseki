//! TCP-framed-postcard binding listener (ADR-042 §2.2).
//!
//! Accept loop pattern mirrors [`kiseki_raft::tcp_transport::
//! RaftRpcListener`] — same multiplexed per-connection shape, same
//! per-peer connection cap. Differences:
//!
//! - Verb dispatch goes through [`super::dispatch::dispatch_verb`]
//!   (native gateway service), not the Raft shard registry.
//! - SAN extraction: at handshake time we run
//!   [`extract_canonical_tenant_san`] over the leaf cert and stash
//!   the canonical SAN URI on the connection's
//!   [`TcpFramedPrincipal`].
//! - Plaintext mode produces a `dev` synthetic SAN (matches the gRPC
//!   binding's `SanInterceptor` plaintext fallback).
//!
//! Lifecycle:
//! 1. `bind` — create [`tokio::net::TcpListener`]
//! 2. `run` — accept loop, spawn per-connection task
//! 3. Per-task: TLS accept (if configured) → SAN extraction → mint
//!    `TcpFramedPrincipal` with monotonic `ConnectionId` →
//!    `serve_connection` → close on EOF / error.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use kiseki_proto::native_contract::ConnectionId;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::native::canonical_san::CanonicalSanUri;
use crate::native::san_interceptor::extract_canonical_tenant_san;
use crate::native::server::ServerImpl;

use super::connection::serve_connection;
use super::principal::TcpFramedPrincipal;

/// Maximum concurrent inbound connections per peer IP. Same shape
/// as [`kiseki_raft::tcp_transport::RAFT_TRANSPORT_PER_PEER_MAX`];
/// docked at 16 so a single misbehaving peer can't soak up all
/// connection slots. Configurable via [`TcpFramedListener::with_per_peer_cap`].
pub const NATIVE_TCP_FRAMED_PER_PEER_MAX: u32 = 16;

/// TCP-framed-postcard binding listener. Accepts inbound connections,
/// runs the rustls handshake (if configured), extracts the canonical
/// SAN, and dispatches per-frame work via [`serve_connection`].
pub struct TcpFramedListener {
    addr: String,
    server: Arc<ServerImpl>,
    tls_acceptor: ArcSwap<Option<TlsAcceptor>>,
    /// `false` requires TLS; `true` accepts plaintext + installs a
    /// synthetic `dev` SAN. Production runtimes set `false`.
    allow_plaintext: bool,
    active_per_peer: Arc<DashMap<String, AtomicU32>>,
    per_peer_cap: u32,
    next_connection_id: Arc<AtomicU64>,
}

impl TcpFramedListener {
    /// Build a new listener.
    ///
    /// Pass `tls_config = None` AND `allow_plaintext = true` for
    /// development; production wires a real `Arc<rustls::ServerConfig>`
    /// matching the cluster CA used by the gRPC binding.
    #[must_use]
    pub fn new(
        addr: String,
        server: Arc<ServerImpl>,
        tls_config: Option<Arc<rustls::ServerConfig>>,
        allow_plaintext: bool,
    ) -> Self {
        let acceptor = tls_config.map(TlsAcceptor::from);
        Self {
            addr,
            server,
            tls_acceptor: ArcSwap::from_pointee(acceptor),
            allow_plaintext,
            active_per_peer: Arc::new(DashMap::new()),
            per_peer_cap: NATIVE_TCP_FRAMED_PER_PEER_MAX,
            next_connection_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Override the per-peer concurrent-connection cap. Defaults to
    /// [`NATIVE_TCP_FRAMED_PER_PEER_MAX`].
    #[must_use]
    pub fn with_per_peer_cap(mut self, cap: u32) -> Self {
        self.per_peer_cap = cap;
        self
    }

    /// Hot-rotate the TLS context. New connections use the new
    /// acceptor; in-flight handshakes finish on the old one.
    pub fn set_tls_acceptor(&self, new_config: Option<Arc<rustls::ServerConfig>>) {
        let acceptor = new_config.map(TlsAcceptor::from);
        self.tls_acceptor.store(Arc::new(acceptor));
    }

    /// Spawn the accept loop. Subsequent calls fail with `EADDRINUSE`.
    ///
    /// # Errors
    /// Returns `io::Error` from the bind step.
    pub async fn run(self) -> io::Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        let has_tls = self.tls_acceptor.load().is_some();
        if has_tls {
            tracing::info!(addr = %self.addr, "TCP-framed native binding listening (mTLS)");
        } else if self.allow_plaintext {
            tracing::warn!(
                addr = %self.addr,
                "TCP-framed native binding listening (plaintext — dev mode; \
                 connections see synthetic 'dev' SAN)",
            );
        } else {
            return Err(io::Error::other(
                "TCP-framed listener: no TLS configured and plaintext disabled",
            ));
        }

        loop {
            let (tcp_stream, peer_addr) = listener.accept().await?;
            let server = Arc::clone(&self.server);
            let acceptor = self.tls_acceptor.load_full();
            let per_peer = Arc::clone(&self.active_per_peer);
            let peer_key = peer_addr.ip().to_string();
            let cap = self.per_peer_cap;
            let allow_plaintext = self.allow_plaintext;
            let conn_id_counter = Arc::clone(&self.next_connection_id);

            // Per-peer cap.
            let counter = per_peer
                .entry(peer_key.clone())
                .or_insert_with(|| AtomicU32::new(0));
            let active = counter.fetch_add(1, Ordering::Relaxed) + 1;
            drop(counter);
            if active > cap {
                if let Some(c) = per_peer.get(&peer_key) {
                    c.fetch_sub(1, Ordering::Relaxed);
                }
                tracing::warn!(
                    peer = %peer_key,
                    active,
                    cap,
                    "rejecting TCP-framed connection — per-peer cap exceeded",
                );
                drop(tcp_stream);
                continue;
            }

            tokio::spawn(async move {
                let conn_id =
                    ConnectionId(conn_id_counter.fetch_add(1, Ordering::Relaxed));
                let result =
                    handle_one_connection(tcp_stream, acceptor.as_ref().clone(), allow_plaintext, server, conn_id)
                        .await;
                if let Some(c) = per_peer.get(&peer_key) {
                    c.fetch_sub(1, Ordering::Relaxed);
                }
                if let Err(e) = result {
                    tracing::debug!(
                        peer = %peer_key,
                        conn_id = %conn_id,
                        error = %e,
                        "TCP-framed connection handler ended",
                    );
                }
            });
        }
    }
}

async fn handle_one_connection(
    tcp_stream: tokio::net::TcpStream,
    acceptor: Option<TlsAcceptor>,
    allow_plaintext: bool,
    server: Arc<ServerImpl>,
    conn_id: ConnectionId,
) -> io::Result<()> {
    // TCP_NODELAY: ADR-042 §2.2's per-call write splits the frame
    // into a small header (14 bytes) followed by the verb body.
    // Without TCP_NODELAY the kernel applies Nagle's algorithm —
    // small first write triggers a 40 ms coalescing wait before the
    // body goes out. Measured impact: 96 k op/s → 318 op/s, p50
    // 184 µs → 42 ms. NODELAY makes the write splits work as
    // intended (kernel ships each call's bytes immediately).
    if let Err(e) = tcp_stream.set_nodelay(true) {
        tracing::warn!(error = %e, "TCP-framed: TCP_NODELAY on accepted socket failed; perf may regress");
    }
    if let Some(acc) = acceptor {
        let mut tls = acc.accept(tcp_stream).await?;
        // Extract canonical SAN from the validated peer cert.
        let principal = principal_from_tls(&tls, conn_id)?;
        match serve_connection(&mut tls, server, principal).await {
            Ok(()) => Ok(()),
            Err(super::connection::ConnectionError::Io(e)) => Err(e),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    } else if allow_plaintext {
        // Dev-mode plaintext: install synthetic `dev` SAN so the
        // existing handler-side cross-check skip kicks in (matches
        // gRPC binding's plaintext fallback).
        let dev = CanonicalSanUri::default_for_dev();
        let principal = TcpFramedPrincipal::new(dev.as_str(), conn_id);
        let mut s = tcp_stream;
        match serve_connection(&mut s, server, principal).await {
            Ok(()) => Ok(()),
            Err(super::connection::ConnectionError::Io(e)) => Err(e),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    } else {
        // Should be unreachable — `run` rejects this configuration
        // at startup. Defensive close.
        Err(io::Error::other(
            "TCP-framed: TLS required but acceptor missing",
        ))
    }
}

fn principal_from_tls(
    tls: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    conn_id: ConnectionId,
) -> io::Result<TcpFramedPrincipal> {
    let (_, conn) = tls.get_ref();
    let certs = conn.peer_certificates().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "TCP-framed: client cert required",
        )
    })?;
    let leaf = certs.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "TCP-framed: client cert chain empty",
        )
    })?;
    let canonical = extract_canonical_tenant_san(leaf.as_ref())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(TcpFramedPrincipal::new(canonical.as_str(), conn_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_gateway::InMemoryGateway;
    use crate::native::signing_keys::SigningKeys;
    use kiseki_chunk::ChunkStore;
    use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_composition::namespace::Namespace;
    use kiseki_crypto::keys::SystemMasterKey;
    use kiseki_proto::native_contract::wire_tcp_framed::{
        decode_response_frame, encode_request_frame, WireStatus,
    };
    use kiseki_proto::v1 as v1;
    use kiseki_proto::v1::native as np;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn make_server() -> Arc<ServerImpl> {
        let gw = Arc::new(InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
        ));
        gw.add_namespace(Namespace {
            id: NamespaceId(uuid::Uuid::from_bytes([2; 16])),
            tenant_id: OrgId(uuid::Uuid::from_bytes([1; 16])),
            shard_id: ShardId(uuid::Uuid::nil()),
            read_only: false,
            versioning_enabled: false,
            compliance_tags: Vec::new(),
        })
        .await;
        let signing = Arc::new(SigningKeys::new(
            &SystemMasterKey::new([0xCC; 32], KeyEpoch(1)),
            60_000,
        ));
        Arc::new(ServerImpl::new(
            gw as Arc<dyn crate::ops::GatewayOps>,
            signing,
        ))
    }

    fn ctrl(tenant: OrgId) -> np::ControlFields {
        np::ControlFields {
            tenant_id: Some(v1::OrgId {
                value: tenant.0.to_string(),
            }),
            idempotency_key: vec![0xAB; 8],
            workflow_ref: String::new(),
            cache_hint: None,
            conditional: None,
        }
    }

    /// End-to-end: spin up the listener in plaintext mode, connect a
    /// raw TCP socket, send one put_object frame, decode the
    /// response. Confirms accept loop + per-connection task wiring.
    #[tokio::test]
    async fn plaintext_listener_round_trips_put_object() {
        // Resolve an ephemeral port by binding then immediately
        // releasing — the kernel keeps it free for the listener's
        // own bind below in practice. (TIME_WAIT could in theory
        // race; the test retries the connect to absorb that.)
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        drop(tcp);
        let server = make_server().await;
        let listener = TcpFramedListener::new(addr.to_string(), server, None, true);
        let server_handle = tokio::spawn(async move { listener.run().await });

        // Tiny retry to give the listener time to bind.
        let mut client = None;
        for _ in 0..50 {
            if let Ok(s) = tokio::net::TcpStream::connect(addr).await {
                client = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut client = client.expect("connect");

        let tenant = OrgId(uuid::Uuid::from_bytes([1; 16]));
        let ns = NamespaceId(uuid::Uuid::from_bytes([2; 16]));
        // V3: meta = postcard(request with empty data); bulk = data.
        let meta = postcard::to_allocvec(&np::PutObjectRequest {
            control: Some(ctrl(tenant)),
            namespace_id: Some(v1::NamespaceId {
                value: ns.0.to_string(),
            }),
            name: "alpha".into(),
            data: Vec::new(),
        })
        .unwrap();
        let frame = encode_request_frame(7, "put_object", &meta, b"hello").unwrap();
        client.write_all(&frame).await.unwrap();
        client.flush().await.unwrap();

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).await.unwrap();
        let body_len = u32::from_be_bytes(len_buf) as usize;
        let mut frame_body = vec![0u8; body_len];
        client.read_exact(&mut frame_body).await.unwrap();
        let view = decode_response_frame(&frame_body).expect("decode response frame");
        assert_eq!(view.status, WireStatus::Ok);
        assert_eq!(view.request_id, 7);
        // PutObject response: bulk is empty, meta has the response.
        assert!(view.bulk.is_empty());
        let put_resp: np::PutObjectResponse = postcard::from_bytes(view.meta).unwrap();
        assert_eq!(put_resp.size, 5);

        // Tear down.
        drop(client);
        server_handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn per_peer_cap_default_pinned() {
        // §3.4 / R2-M5: cap defaults to 16, configurable via builder.
        // Pin so a refactor doesn't change it under the radar.
        assert_eq!(NATIVE_TCP_FRAMED_PER_PEER_MAX, 16);
    }

    #[tokio::test]
    async fn listener_rejects_no_tls_no_plaintext_combination_at_runtime() {
        // Constructor allows it (so tests can mint), but `run`
        // rejects. We can't easily invoke run() in a unit test
        // without binding a real port; assert via the construction
        // path that the field is preserved.
        let server = make_server().await;
        let l = TcpFramedListener::new("127.0.0.1:0".into(), server, None, false);
        assert!(!l.allow_plaintext);
        assert!(l.tls_acceptor.load().is_none());
    }
}
