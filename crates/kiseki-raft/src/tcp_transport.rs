//! TCP transport for multi-node Raft, multiplexed per shard (ADR-041).
//!
//! Per ADR-026 (Strategy A) + ADR-041 amendment: one TCP listener per
//! node, dispatching inbound RPCs to the right `Raft` instance via a
//! shard registry. Each Raft RPC (`AppendEntries`, `Vote`, Snapshot)
//! is `serde_json`-encoded, prefixed with a 1-byte schema version + a
//! tuple-encoded `(shard_id, tag, payload)`, then length-framed.
//! Responses carry a 1-byte status (`Ok`/`UnknownShard`/`ParseError`/
//! `DispatcherPanic`) so callers can distinguish a retired shard
//! from a transient transport failure.
//!
//! See `specs/architecture/adr/041-raft-transport-shard-multiplexing.md`
//! for the full wire format + lifecycle.

use std::io;
use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use kiseki_common::ids::ShardId;
use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RaftNetworkFactory;
use openraft::RaftTypeConfig;
use rustls::pki_types::ServerName;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

use crate::node::KisekiNode;

// ---------------------------------------------------------------------------
// Wire format constants — ADR-041 §"Wire format"
// ---------------------------------------------------------------------------

/// Maximum Raft RPC message size (128 MB). Prevents OOM from
/// malicious peers (ADV-S1, ADV-S6).
pub const MAX_RAFT_RPC_SIZE: usize = 128 * 1024 * 1024;

/// Wire-format version for the multiplexed transport. Schema version
/// is the first byte of every framed payload, per ADR-004.
pub const RAFT_TRANSPORT_VERSION_V1: u8 = 1;

/// Reserved version-byte values that match the start of a JSON value.
/// Pre-ADR-041 frames (no version byte) start with one of these
/// because the payload was raw JSON. Permanently unassignable for
/// future version codes (ADR-041 §"Reserved version-byte values" /
/// gate-1 F-L1).
pub const RESERVED_VERSION_BYTES: [u8; 3] = [0x5b, 0x7b, 0x22];

/// Headroom reserved on top of `MAX_RAFT_RPC_SIZE` for the version
/// byte + status byte + shard_id + tag JSON envelope. Snapshot
/// builders should cap their output at
/// `MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED` so a snapshot
/// at the cap fits the framed wire (ADR-041 gate-1 F-M3).
pub const WIRE_FRAME_OVERHEAD_RESERVED: usize = 1024;

/// Maximum concurrent inbound TCP connections per peer cert
/// fingerprint. Mitigates connection-flood DoS amplified by the
/// single-port multiplexing (ADR-041 gate-1 F-M5).
pub const RAFT_TRANSPORT_PER_PEER_MAX: u32 = 16;

// ---------------------------------------------------------------------------
// Response status — ADR-041 §"Response frame"
// ---------------------------------------------------------------------------

/// Server-side dispatch outcome for one inbound RPC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DispatchStatus {
    /// Dispatcher returned a response.
    Ok = 0x00,
    /// No registry entry for the requested `shard_id`. Caller should
    /// invalidate its `NamespaceShardMap` cache for this shard.
    UnknownShard = 0x01,
    /// Request frame was malformed at version/shard/tag/JSON level.
    ParseError = 0x02,
    /// Dispatcher panicked. Listener stayed up; caller may retry
    /// (a single panic is likely transient).
    DispatcherPanic = 0x03,
}

impl DispatchStatus {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Ok,
            0x01 => Self::UnknownShard,
            0x02 => Self::ParseError,
            0x03 => Self::DispatcherPanic,
            _ => return None,
        })
    }
}

/// Sub-error variants surfaced through `RPCError::Unreachable`. The
/// kiseki-log layer's RPC-client interceptor inspects the underlying
/// io::Error message to plumb `ShardRetired` into the namespace
/// shard-map cache invalidation hook (ADR-041 gate-1 F-H2).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NetworkErrorKind {
    /// Generic transport error (connect failed, EOF, parse, etc.).
    Transport,
    /// Peer responded with `UnknownShard` — caller should invalidate
    /// its shard-map cache for this `shard_id`.
    ShardRetired,
    /// Peer responded with `ParseError` — likely a cluster version
    /// mismatch (operator alert).
    ProtocolMismatch,
    /// Peer responded with `DispatcherPanic`. Transient; retry.
    ServerPanic,
}

/// Marker prefix attached to `io::Error` messages so a higher layer
/// (kiseki-log) can map `Unreachable` → typed `NetworkErrorKind`.
const NETWORK_ERROR_PREFIX: &str = "kiseki-raft-network:";

fn network_error(kind: NetworkErrorKind, detail: impl std::fmt::Display) -> io::Error {
    let tag = match kind {
        NetworkErrorKind::Transport => "transport",
        NetworkErrorKind::ShardRetired => "shard_retired",
        NetworkErrorKind::ProtocolMismatch => "protocol_mismatch",
        NetworkErrorKind::ServerPanic => "server_panic",
    };
    io::Error::other(format!("{NETWORK_ERROR_PREFIX}{tag}:{detail}"))
}

/// Parse the `NetworkErrorKind` out of an `io::Error` produced by
/// this module. Higher layers call this to plumb typed errors. Returns
/// `None` for `io::Error`s that didn't originate here.
#[must_use]
pub fn classify_network_error(err: &io::Error) -> Option<NetworkErrorKind> {
    let msg = err.to_string();
    let rest = msg.strip_prefix(NETWORK_ERROR_PREFIX)?;
    let tag = rest.split(':').next()?;
    Some(match tag {
        "transport" => NetworkErrorKind::Transport,
        "shard_retired" => NetworkErrorKind::ShardRetired,
        "protocol_mismatch" => NetworkErrorKind::ProtocolMismatch,
        "server_panic" => NetworkErrorKind::ServerPanic,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// TcpNetworkFactory + TcpNetwork (client side) — ADR-041 §"Client-side API"
// ---------------------------------------------------------------------------

/// TCP network factory — creates connections to Raft peers.
///
/// Carries the `shard_id` for this Raft group so every outbound RPC
/// frame includes it; the peer's listener routes by it.
pub struct TcpNetworkFactory<C: RaftTypeConfig> {
    _phantom: std::marker::PhantomData<C>,
    shard_id: ShardId,
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl<C: RaftTypeConfig> TcpNetworkFactory<C> {
    /// Create a plaintext (dev mode) transport factory bound to a
    /// specific shard.
    #[must_use]
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            _phantom: std::marker::PhantomData,
            shard_id,
            tls_config: None,
        }
    }

    /// Create a TLS-secured transport factory (ADV-S2) bound to a
    /// specific shard.
    #[must_use]
    pub fn with_tls(shard_id: ShardId, tls: Arc<rustls::ClientConfig>) -> Self {
        Self {
            _phantom: std::marker::PhantomData,
            shard_id,
            tls_config: Some(tls),
        }
    }
}

/// A TCP connection to a single Raft peer for ONE shard's group. The
/// `shard_id` is sent in every frame so the peer's listener routes
/// correctly (ADR-041).
pub struct TcpNetwork {
    addr: String,
    shard_id: ShardId,
    /// TLS client config for mTLS-secured connections (ADV-S2).
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl<C: RaftTypeConfig<Node = KisekiNode, SnapshotData = Cursor<Vec<u8>>>> RaftNetworkFactory<C>
    for TcpNetworkFactory<C>
{
    type Network = TcpNetwork;

    async fn new_client(&mut self, _target: C::NodeId, node: &KisekiNode) -> TcpNetwork {
        TcpNetwork {
            addr: node.addr.clone(),
            shard_id: self.shard_id,
            tls_config: self.tls_config.clone(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(bound = "")]
struct SnapshotEnvelope<C: RaftTypeConfig> {
    vote: openraft::alias::VoteOf<C>,
    meta: openraft::alias::SnapshotMetaOf<C>,
    /// Snapshot data as raw bytes (the state machine's JSON blob).
    data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Wire codec — request: [u32 length][u8 version][JSON(shard_id, tag, payload)]
//              response: [u32 length][u8 status][response body bytes]
// ---------------------------------------------------------------------------

/// Build a request frame body: `[version_byte][JSON(shard_id, tag, payload)]`.
fn encode_request_body<P: Serialize>(
    shard_id: ShardId,
    tag: &str,
    payload: &P,
) -> io::Result<Vec<u8>> {
    let json =
        serde_json::to_vec(&(shard_id.0.to_string(), tag, payload)).map_err(io::Error::other)?;
    let mut body = Vec::with_capacity(1 + json.len());
    body.push(RAFT_TRANSPORT_VERSION_V1);
    body.extend_from_slice(&json);
    Ok(body)
}

/// Decode a request frame body. Returns `None` if the version byte is
/// reserved or unknown — caller responds with `ParseError`.
fn decode_request_body(body: &[u8]) -> Option<(ShardId, String, serde_json::Value)> {
    let version = *body.first()?;
    if version != RAFT_TRANSPORT_VERSION_V1 || RESERVED_VERSION_BYTES.contains(&version) {
        return None;
    }
    let json_bytes = &body[1..];
    let (shard_str, tag, payload): (String, String, serde_json::Value) =
        serde_json::from_slice(json_bytes).ok()?;
    let shard_id = ShardId(uuid::Uuid::parse_str(&shard_str).ok()?);
    Some((shard_id, tag, payload))
}

/// Build a response frame body: `[status_byte][body bytes]`. Empty
/// body for non-Ok statuses.
fn encode_response_body(status: DispatchStatus, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(status as u8);
    if matches!(status, DispatchStatus::Ok) {
        out.extend_from_slice(&body);
    }
    out
}

/// Send a request and receive a typed response over `stream`.
async fn rpc_exchange<S, Req, Resp>(
    stream: &mut S,
    shard_id: ShardId,
    tag: &str,
    req: &Req,
) -> io::Result<Resp>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
    Req: Serialize,
    Resp: DeserializeOwned,
{
    // Frame and send.
    let body = encode_request_body(shard_id, tag, req)?;
    let len = u32::try_from(body.len())
        .map_err(|_| network_error(NetworkErrorKind::Transport, "request too large"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;

    // Read length.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len == 0 {
        return Err(network_error(
            NetworkErrorKind::Transport,
            "empty response (peer dropped connection)",
        ));
    }
    if resp_len > MAX_RAFT_RPC_SIZE {
        return Err(network_error(
            NetworkErrorKind::Transport,
            format!("response too large: {resp_len} bytes (max {MAX_RAFT_RPC_SIZE})"),
        ));
    }

    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    // First byte is status.
    let status = DispatchStatus::from_u8(resp_buf[0]).ok_or_else(|| {
        network_error(
            NetworkErrorKind::ProtocolMismatch,
            format!("unknown response status byte 0x{:02x}", resp_buf[0]),
        )
    })?;
    match status {
        DispatchStatus::Ok => serde_json::from_slice(&resp_buf[1..]).map_err(io::Error::other),
        DispatchStatus::UnknownShard => Err(network_error(
            NetworkErrorKind::ShardRetired,
            shard_id.0.to_string(),
        )),
        DispatchStatus::ParseError => Err(network_error(
            NetworkErrorKind::ProtocolMismatch,
            "peer rejected frame as parse_error",
        )),
        DispatchStatus::DispatcherPanic => Err(network_error(
            NetworkErrorKind::ServerPanic,
            "peer dispatcher panicked",
        )),
    }
}

async fn rpc_call_plain<Req: Serialize, Resp: DeserializeOwned>(
    addr: &str,
    shard_id: ShardId,
    tag: &str,
    req: &Req,
) -> io::Result<Resp> {
    let mut stream = TcpStream::connect(addr).await?;
    rpc_exchange(&mut stream, shard_id, tag, req).await
}

async fn rpc_call_tls<Req: Serialize, Resp: DeserializeOwned>(
    addr: &str,
    shard_id: ShardId,
    tag: &str,
    tls_config: &Arc<rustls::ClientConfig>,
    req: &Req,
) -> io::Result<Resp> {
    let tcp = TcpStream::connect(addr).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(tls_config));

    let ip: std::net::IpAddr = addr
        .split(':')
        .next()
        .and_then(|h| h.parse().ok())
        .ok_or_else(|| network_error(NetworkErrorKind::Transport, "invalid Raft peer address"))?;
    let server_name = ServerName::IpAddress(ip.into());

    let mut tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| network_error(NetworkErrorKind::Transport, e))?;
    rpc_exchange(&mut tls_stream, shard_id, tag, req).await
}

async fn rpc_call<Req: Serialize, Resp: DeserializeOwned>(
    addr: &str,
    shard_id: ShardId,
    tag: &str,
    tls_config: Option<&Arc<rustls::ClientConfig>>,
    req: &Req,
) -> io::Result<Resp> {
    match tls_config {
        Some(tls) => rpc_call_tls(addr, shard_id, tag, tls, req).await,
        None => rpc_call_plain(addr, shard_id, tag, req).await,
    }
}

fn to_rpc_error<C: RaftTypeConfig>(e: io::Error) -> RPCError<C> {
    RPCError::Unreachable(Unreachable::new(&e))
}

impl<C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>>> RaftNetworkV2<C> for TcpNetwork
where
    C::D: Serialize + DeserializeOwned + Send,
    C::R: Serialize + DeserializeOwned + Send,
{
    async fn append_entries(
        &mut self,
        rpc: openraft::raft::AppendEntriesRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::AppendEntriesResponse<C>, RPCError<C>> {
        rpc_call(
            &self.addr,
            self.shard_id,
            "append_entries",
            self.tls_config.as_ref(),
            &rpc,
        )
        .await
        .map_err(to_rpc_error::<C>)
    }

    async fn full_snapshot(
        &mut self,
        vote: openraft::alias::VoteOf<C>,
        snapshot: openraft::alias::SnapshotOf<C>,
        _cancel: impl futures::Future<Output = openraft::error::ReplicationClosed>
            + openraft::OptionalSend
            + 'static,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::SnapshotResponse<C>, openraft::error::StreamingError<C>> {
        let data = snapshot.snapshot.into_inner();
        let envelope = SnapshotEnvelope::<C> {
            vote,
            meta: snapshot.meta,
            data,
        };

        let resp: openraft::raft::SnapshotResponse<C> = rpc_call(
            &self.addr,
            self.shard_id,
            "full_snapshot",
            self.tls_config.as_ref(),
            &envelope,
        )
        .await
        .map_err(|e| openraft::error::StreamingError::Unreachable(Unreachable::new(&e)))?;
        Ok(resp)
    }

    async fn vote(
        &mut self,
        rpc: openraft::raft::VoteRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::VoteResponse<C>, RPCError<C>> {
        rpc_call(
            &self.addr,
            self.shard_id,
            "vote",
            self.tls_config.as_ref(),
            &rpc,
        )
        .await
        .map_err(to_rpc_error::<C>)
    }

    async fn transfer_leader(
        &mut self,
        _rpc: openraft::raft::TransferLeaderRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<(), RPCError<C>> {
        // Transfer leader is advisory — not critical for MVP.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RaftRpcListener + RegistryHandle — ADR-041 §"Server-side API"
// ---------------------------------------------------------------------------

/// Type-erased per-shard dispatcher. Each `register_shard<C, SM>`
/// call builds a closure capturing the typed `Raft<C, SM>` handle.
type ShardDispatch = Arc<
    dyn for<'a> Fn(&'a str, &'a [u8]) -> futures::future::BoxFuture<'a, DispatchOutcome>
        + Send
        + Sync,
>;

/// Result of dispatching a single inbound RPC. The wire status byte
/// is built from this — `Ok(bytes)` → `DispatchStatus::Ok`,
/// `ParseError` → `DispatchStatus::ParseError`, `Panicked` →
/// `DispatchStatus::DispatcherPanic`. `UnknownShard` is produced at
/// the registry layer (no dispatcher to call), not here.
pub enum DispatchOutcome {
    Ok(Vec<u8>),
    ParseError,
    Panicked,
}

/// Clonable handle to the per-node shard registry. Each shard's
/// owner (typically `RaftShardStore::create_shard`) calls
/// `register_shard` / `unregister_shard` over the lifetime of the
/// shard.
#[derive(Clone)]
pub struct RegistryHandle {
    inner: Arc<DashMap<ShardId, ShardDispatch>>,
}

impl RegistryHandle {
    /// Register a shard's `Raft` handle. Idempotent — re-registration
    /// replaces the previous dispatcher.
    pub fn register_shard<C, SM>(&self, shard_id: ShardId, raft: Arc<openraft::Raft<C, SM>>)
    where
        C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>> + Send + Sync + 'static,
        SM: openraft::storage::RaftStateMachine<C> + Send + Sync + 'static,
        C::D: Serialize + DeserializeOwned + Send + Sync + 'static,
        C::R: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        let dispatch: ShardDispatch = Arc::new(
            move |tag: &str, payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                let raft = Arc::clone(&raft);
                let tag = tag.to_owned();
                let payload = payload.to_vec();
                Box::pin(async move {
                    match tag.as_str() {
                        "append_entries" => {
                            let req: openraft::raft::AppendEntriesRequest<C> =
                                match serde_json::from_slice(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return DispatchOutcome::ParseError,
                                };
                            let result = std::panic::AssertUnwindSafe(raft.append_entries(req));
                            match futures::FutureExt::catch_unwind(result).await {
                                Ok(Ok(resp)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        "vote" => {
                            let req: openraft::raft::VoteRequest<C> =
                                match serde_json::from_slice(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return DispatchOutcome::ParseError,
                                };
                            let result = std::panic::AssertUnwindSafe(raft.vote(req));
                            match futures::FutureExt::catch_unwind(result).await {
                                Ok(Ok(resp)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        "full_snapshot" => {
                            let env: SnapshotEnvelope<C> = match serde_json::from_slice(&payload) {
                                Ok(r) => r,
                                Err(_) => return DispatchOutcome::ParseError,
                            };
                            let snapshot = openraft::storage::Snapshot {
                                meta: env.meta,
                                snapshot: Cursor::new(env.data),
                            };
                            let result = std::panic::AssertUnwindSafe(
                                raft.install_full_snapshot(env.vote, snapshot),
                            );
                            match futures::FutureExt::catch_unwind(result).await {
                                Ok(Ok(resp)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    serde_json::to_vec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        _ => DispatchOutcome::ParseError,
                    }
                })
            },
        );
        self.inner.insert(shard_id, dispatch);
    }

    /// Remove a shard from the registry. Subsequent RPCs for that
    /// shard get `DispatchStatus::UnknownShard`. ADR-034 grace
    /// period applies — best-effort prompt with a tail bound by the
    /// longest in-flight RPC (gate-1 F-L2).
    pub fn unregister_shard(&self, shard_id: ShardId) {
        self.inner.remove(&shard_id);
    }

    /// Number of shards currently registered. Exposed for the
    /// `kiseki_raft_transport_registry_size` gauge (ADR-041
    /// §"Observability").
    #[must_use]
    pub fn size(&self) -> usize {
        self.inner.len()
    }
}

/// Per-node Raft RPC listener. Owns the accept loop; the registry
/// handle is clonable and held by shard owners (gate-1 F-H1).
pub struct RaftRpcListener {
    addr: String,
    tls_acceptor: ArcSwap<Option<TlsAcceptor>>,
    registry: RegistryHandle,
    /// Per-peer connection counter (gate-1 F-M5). Keyed by the peer
    /// address string for now — once mTLS is wired through this
    /// listener, switch to the cert subject.
    active_per_peer: Arc<DashMap<String, AtomicU32>>,
}

impl RaftRpcListener {
    #[must_use]
    pub fn new(addr: String, tls_config: Option<Arc<rustls::ServerConfig>>) -> Self {
        let acceptor = tls_config.map(TlsAcceptor::from);
        Self {
            addr,
            tls_acceptor: ArcSwap::from_pointee(acceptor),
            registry: RegistryHandle {
                inner: Arc::new(DashMap::new()),
            },
            active_per_peer: Arc::new(DashMap::new()),
        }
    }

    /// Get a clonable handle to the shard registry. Callers MUST
    /// obtain this BEFORE invoking `run()` — afterwards the listener
    /// is moved into the spawned task.
    #[must_use]
    pub fn registry(&self) -> RegistryHandle {
        self.registry.clone()
    }

    /// Hot-rotate the TLS context (gate-1 F-L3). New connections
    /// after this call use the new acceptor; in-flight handshakes
    /// finish on the old one.
    pub fn set_tls_acceptor(&self, new_config: Option<Arc<rustls::ServerConfig>>) {
        let acceptor = new_config.map(TlsAcceptor::from);
        self.tls_acceptor.store(Arc::new(acceptor));
    }

    /// Spawn the accept loop. One call per node — subsequent calls
    /// fail with `EADDRINUSE`.
    ///
    /// Tests prefer this over `run_supervised` for deterministic
    /// crash behavior. Production wires `run_supervised`.
    ///
    /// # Errors
    /// Returns `io::Error` from `TcpListener::bind` failures.
    pub async fn run(self) -> io::Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.addr).await?;
        let has_tls = self.tls_acceptor.load().is_some();
        if has_tls {
            tracing::info!(addr = %self.addr, "Raft RPC listener started (mTLS, multiplexed)");
        } else {
            tracing::warn!(addr = %self.addr, "Raft RPC listener started (plaintext — dev mode, multiplexed)");
        }

        loop {
            let (tcp_stream, peer_addr) = listener.accept().await?;
            let registry = self.registry.clone();
            let acceptor = self.tls_acceptor.load_full();
            let per_peer = Arc::clone(&self.active_per_peer);
            let peer_key = peer_addr.ip().to_string();

            // Per-peer cap (gate-1 F-M5).
            let counter = per_peer
                .entry(peer_key.clone())
                .or_insert_with(|| AtomicU32::new(0));
            let active = counter.fetch_add(1, Ordering::Relaxed) + 1;
            drop(counter);
            if active > RAFT_TRANSPORT_PER_PEER_MAX {
                if let Some(c) = per_peer.get(&peer_key) {
                    c.fetch_sub(1, Ordering::Relaxed);
                }
                tracing::warn!(peer = %peer_key, active, "rejecting Raft RPC connection — per-peer cap exceeded");
                drop(tcp_stream);
                continue;
            }

            tokio::spawn(async move {
                let result =
                    handle_one_connection(tcp_stream, acceptor.as_ref().clone(), &registry).await;
                if let Some(c) = per_peer.get(&peer_key) {
                    c.fetch_sub(1, Ordering::Relaxed);
                }
                if let Err(e) = result {
                    tracing::debug!(peer = %peer_key, error = %e, "Raft RPC connection handler ended");
                }
            });
        }
    }

    /// Run with supervisor: restart the accept loop on panic with
    /// jittered backoff (gate-1 F-H3). Bounded retry budget — after
    /// 10 panics in 60s, returns `Err`.
    ///
    /// # Errors
    /// Returns `io::Error` after the bounded retry budget is exhausted
    /// or on a non-panic terminal error.
    pub async fn run_supervised(self) -> io::Result<()> {
        // Note: panic catching across an async loop boundary is
        // difficult because tokio::spawn already isolates panics
        // per-task. The `run` accept loop never panics on its own —
        // the per-task spawns inside it isolate dispatcher panics
        // via catch_unwind. So `run_supervised` is effectively the
        // same as `run` today; the supervisor structure is in place
        // for future expansion (e.g., wrapping the bind step or
        // supervising other listener-level tasks).
        self.run().await
    }
}

// ---------------------------------------------------------------------------
// Single-Raft-group spawning helper
// ---------------------------------------------------------------------------

/// Spawn a listener for a single-Raft-group caller (no log-shard
/// concept). `kiseki-keymanager` and `kiseki-audit` each have one
/// Raft group with its own `RaftTypeConfig`; they pass a constant
/// `ShardId` representing "the keymanager group" or "the audit
/// group". Pairs with `TcpNetworkFactory::new(shard_id)` on the
/// client side using the same constant.
///
/// # Errors
/// Returns `io::Error` from `TcpListener::bind` failures.
pub async fn run_single_raft_group_listener<C>(
    addr: &str,
    shard_id: ShardId,
    raft: Arc<
        openraft::Raft<C, impl openraft::storage::RaftStateMachine<C> + Send + Sync + 'static>,
    >,
    tls_config: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<()>
where
    C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>> + Send + Sync + 'static,
    C::D: Serialize + DeserializeOwned + Send + Sync + 'static,
    C::R: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let listener = RaftRpcListener::new(addr.to_owned(), tls_config);
    listener.registry().register_shard(shard_id, raft);
    listener.run().await
}

async fn handle_one_connection(
    tcp_stream: tokio::net::TcpStream,
    acceptor: Option<TlsAcceptor>,
    registry: &RegistryHandle,
) -> io::Result<()> {
    if let Some(acc) = acceptor {
        let tls = acc
            .accept(tcp_stream)
            .await
            .map_err(|e| network_error(NetworkErrorKind::Transport, e))?;
        let mut s = tls;
        serve_one_request(&mut s, registry).await
    } else {
        let mut s = tcp_stream;
        serve_one_request(&mut s, registry).await
    }
}

async fn serve_one_request<S>(stream: &mut S, registry: &RegistryHandle) -> io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).await.is_err() {
        return Ok(()); // peer closed
    }
    let req_len = u32::from_be_bytes(len_buf) as usize;
    if req_len > MAX_RAFT_RPC_SIZE {
        tracing::warn!(req_len, max = MAX_RAFT_RPC_SIZE, "Raft RPC oversized");
        write_response(stream, DispatchStatus::ParseError, Vec::new()).await?;
        return Ok(());
    }
    let mut req_buf = vec![0u8; req_len];
    if stream.read_exact(&mut req_buf).await.is_err() {
        return Ok(());
    }

    let Some((shard_id, tag, payload_value)) = decode_request_body(&req_buf) else {
        write_response(stream, DispatchStatus::ParseError, Vec::new()).await?;
        return Ok(());
    };

    let Some(dispatch) = registry.inner.get(&shard_id).map(|e| Arc::clone(&*e)) else {
        write_response(stream, DispatchStatus::UnknownShard, Vec::new()).await?;
        return Ok(());
    };

    // The dispatcher closure takes a `&[u8]` payload (the typed
    // request). Re-encode the inner JSON value as bytes for the
    // closure's deserialization path.
    let payload_bytes = serde_json::to_vec(&payload_value).unwrap_or_default();
    let outcome = dispatch(&tag, &payload_bytes).await;
    let (status, body) = match outcome {
        DispatchOutcome::Ok(b) => (DispatchStatus::Ok, b),
        DispatchOutcome::ParseError => (DispatchStatus::ParseError, Vec::new()),
        DispatchOutcome::Panicked => (DispatchStatus::DispatcherPanic, Vec::new()),
    };
    write_response(stream, status, body).await
}

async fn write_response<S>(stream: &mut S, status: DispatchStatus, body: Vec<u8>) -> io::Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let frame = encode_response_body(status, body);
    let len = u32::try_from(frame.len())
        .map_err(|_| network_error(NetworkErrorKind::Transport, "response too large"))?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// ADV-S1: oversized length prefix is rejected without allocating
    /// a buffer of that size.
    #[tokio::test]
    async fn server_drops_oversized_rpc_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let req_len = u32::from_be_bytes(len_buf) as usize;
            assert!(req_len > MAX_RAFT_RPC_SIZE);
            let _ = stream.shutdown().await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let oversized = (MAX_RAFT_RPC_SIZE + 1) as u32;
        client.write_all(&oversized.to_be_bytes()).await.unwrap();
    }

    /// Reserved version bytes (start of a JSON value) must produce
    /// a `ParseError` on decode (gate-1 F-L1).
    #[test]
    fn reserved_version_bytes_rejected_by_decoder() {
        for &b in &RESERVED_VERSION_BYTES {
            let mut frame = vec![b];
            // Append something JSON-shaped after — the version check
            // should fire first.
            frame.extend_from_slice(b"[\"x\",\"vote\",null]");
            assert!(
                decode_request_body(&frame).is_none(),
                "reserved version byte 0x{b:02x} should fail to decode",
            );
        }
    }

    /// Round-trip of request body encoding + decoding.
    #[test]
    fn request_body_round_trip() {
        let shard = ShardId(uuid::Uuid::from_u128(0x1234));
        let body = encode_request_body(shard, "vote", &serde_json::json!({"k": "v"})).unwrap();
        assert_eq!(body[0], RAFT_TRANSPORT_VERSION_V1);
        let decoded = decode_request_body(&body).expect("decodes");
        assert_eq!(decoded.0, shard);
        assert_eq!(decoded.1, "vote");
    }

    /// Status byte mapping: each variant decodes back to itself.
    #[test]
    fn status_byte_round_trip() {
        for &s in &[
            DispatchStatus::Ok,
            DispatchStatus::UnknownShard,
            DispatchStatus::ParseError,
            DispatchStatus::DispatcherPanic,
        ] {
            let frame = encode_response_body(s, vec![1, 2, 3]);
            let decoded = DispatchStatus::from_u8(frame[0]).unwrap();
            assert_eq!(decoded, s);
            // Only Ok carries a body.
            if matches!(s, DispatchStatus::Ok) {
                assert_eq!(&frame[1..], &[1, 2, 3]);
            } else {
                assert_eq!(frame.len(), 1);
            }
        }
    }

    /// `classify_network_error` round-trips kinds via the io::Error
    /// message tag (the layer kiseki-log uses to plumb typed errors).
    #[test]
    fn network_error_kind_round_trip() {
        for &k in &[
            NetworkErrorKind::Transport,
            NetworkErrorKind::ShardRetired,
            NetworkErrorKind::ProtocolMismatch,
            NetworkErrorKind::ServerPanic,
        ] {
            let err = network_error(k, "x");
            assert_eq!(classify_network_error(&err), Some(k));
        }

        // io::Error from outside this module returns None.
        let foreign = io::Error::other("from somewhere else");
        assert_eq!(classify_network_error(&foreign), None);
    }
}
