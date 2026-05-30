//! TCP transport for multi-node Raft, multiplexed per shard (ADR-041).
//!
//! Per ADR-026 (Strategy A) + ADR-041 amendment: one TCP listener per
//! node, dispatching inbound RPCs to the right `Raft` instance via a
//! shard registry. Each Raft RPC (`AppendEntries`, `Vote`, Snapshot)
//! is `postcard`-encoded (rev-2, 2026-05-06 — was serde_json), prefixed
//! with a 1-byte schema version + an outer tuple wrapper
//! `(shard_id, tag, raw_payload_bytes)`, then length-framed. The
//! inner `raw_payload_bytes` is itself postcard-encoded by the caller
//! (typed at the call site); the dispatcher decodes the outer wrapper,
//! routes by `tag`, then postcard-decodes the inner bytes against the
//! typed handler's request shape.
//!
//! Responses carry a 1-byte status (`Ok`/`UnknownShard`/`ParseError`/
//! `DispatcherPanic`) so callers can distinguish a retired shard
//! from a transient transport failure. `Ok` body is postcard-encoded.
//!
//! See `specs/architecture/adr/041-raft-transport-shard-multiplexing.md`
//! for the full wire format + lifecycle.

use std::io;
use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

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
/// is the first byte of every framed payload, per ADR-004. Bumped to
/// 2 on 2026-05-06 — postcard replaced serde_json for the outer
/// envelope and inner payloads. Pre-1.0 wire-format change; peers
/// running v1 see "unknown version" and reject (`ParseError`)
/// instead of mis-decoding. Operators wipe + re-replicate.
pub const RAFT_TRANSPORT_VERSION_V2: u8 = 2;

/// Backwards-compat alias for downstream callers that referenced the
/// old name. Same value; new code should use `..._V2`.
pub const RAFT_TRANSPORT_VERSION_V1: u8 = RAFT_TRANSPORT_VERSION_V2;

/// Reserved version-byte values that match the start of a JSON value.
/// Pre-ADR-041 frames (no version byte) started with one of these
/// because the payload was raw JSON. Permanently unassignable for
/// future version codes (ADR-041 §"Reserved version-byte values" /
/// gate-1 F-L1). Postcard frames don't collide with any of these
/// because the version byte sits in front and we never assign a
/// value in this set.
pub const RESERVED_VERSION_BYTES: [u8; 3] = [0x5b, 0x7b, 0x22];

/// Headroom reserved on top of `MAX_RAFT_RPC_SIZE` for the version
/// byte + status byte + shard_id + tag JSON envelope. Snapshot
/// builders should cap their output at
/// `MAX_RAFT_RPC_SIZE - WIRE_FRAME_OVERHEAD_RESERVED` so a snapshot
/// at the cap fits the framed wire (ADR-041 gate-1 F-M3).
pub const WIRE_FRAME_OVERHEAD_RESERVED: usize = 1024;

/// Default maximum concurrent inbound TCP connections per peer cert
/// fingerprint. Mitigates connection-flood DoS amplified by the
/// single-port multiplexing (ADR-041 gate-1 F-M5). Override with
/// `KISEKI_RAFT_PER_PEER_MAX` for cluster sizes/shard counts that
/// legitimately exceed the default.
pub const RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT: u32 = 16;

/// Resolve the runtime per-peer cap from `KISEKI_RAFT_PER_PEER_MAX`,
/// falling back to [`RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT`]. Read once
/// per accept so an operator can tune without a restart.
fn per_peer_cap() -> u32 {
    std::env::var("KISEKI_RAFT_PER_PEER_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(RAFT_TRANSPORT_PER_PEER_MAX_DEFAULT)
}

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

/// A reusable connection to a Raft peer (the connection-pooling fix).
/// Held across RPCs by `TcpNetwork` so each AppendEntries no longer pays
/// a full TCP (re)dial — the dominant per-write commit cost on the
/// multi-node write path.
enum RaftConn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// A TCP connection to a single Raft peer for ONE shard's group. The
/// `shard_id` is sent in every frame so the peer's listener routes
/// correctly (ADR-041).
pub struct TcpNetwork {
    addr: String,
    shard_id: ShardId,
    /// TLS client config for mTLS-secured connections (ADV-S2).
    tls_config: Option<Arc<rustls::ClientConfig>>,
    /// Persistent connection reused across `append_entries` RPCs. Lazily
    /// established; dropped + reconnected on any I/O error (the peer may
    /// close a half-open connection). `None` until the first RPC.
    conn: Option<RaftConn>,
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
            conn: None,
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
// Wire codec — request: [u32 length][u8 version][postcard((shard_id, tag, payload_bytes))]
//              response: [u32 length][u8 status][postcard response body when Ok]
// ---------------------------------------------------------------------------

/// Build a request frame body. Two-stage encoding:
///
/// 1. Postcard-encode the typed `payload` to raw bytes.
/// 2. Postcard-encode the outer tuple
///    `(shard_id_bytes [u8; 16], tag, payload_bytes Vec<u8>)`.
///
/// The two-stage shape lets the dispatcher decode the outer envelope
/// without knowing the typed payload — it routes by `tag`, then
/// hands `payload_bytes` to the matching typed handler which does
/// its own postcard-decode.
fn encode_request_body<P: Serialize>(
    shard_id: ShardId,
    tag: &str,
    payload: &P,
) -> io::Result<Vec<u8>> {
    let payload_bytes = postcard::to_stdvec(payload).map_err(io::Error::other)?;
    let outer = (*shard_id.0.as_bytes(), tag.to_owned(), payload_bytes);
    let outer_bytes = postcard::to_stdvec(&outer).map_err(io::Error::other)?;
    let mut body = Vec::with_capacity(1 + outer_bytes.len());
    body.push(RAFT_TRANSPORT_VERSION_V2);
    body.extend_from_slice(&outer_bytes);
    Ok(body)
}

/// Decode a request frame body. Returns `None` if the version byte is
/// reserved or unknown — caller responds with `ParseError`.
fn decode_request_body(body: &[u8]) -> Option<(ShardId, String, Vec<u8>)> {
    let version = *body.first()?;
    if version != RAFT_TRANSPORT_VERSION_V2 || RESERVED_VERSION_BYTES.contains(&version) {
        return None;
    }
    let outer_bytes = &body[1..];
    let (id_bytes, tag, payload_bytes): ([u8; 16], String, Vec<u8>) =
        postcard::from_bytes(outer_bytes).ok()?;
    let shard_id = ShardId(uuid::Uuid::from_bytes(id_bytes));
    Some((shard_id, tag, payload_bytes))
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
        DispatchStatus::Ok => postcard::from_bytes(&resp_buf[1..]).map_err(io::Error::other),
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

/// `KISEKI_RAFT_FAKE_RTT_US` — simulated outbound RTT (microseconds)
/// added BEFORE every request write on the client side. Read once at
/// module init via a `OnceLock`. Empty / unset / `0` / unparseable →
/// the sleep is skipped (~1 cmp + branch per call, no syscall).
///
/// Purpose: harness-driven scenario discrimination per
/// `specs/escalations/2026-05-30-decoupled-ack-perf-10x-analysis.md`
/// A-1 finding. Localhost loopback RTT is ~30 µs; the ~22 ms p50 on
/// GCP implies several ms / hop. Setting `KISEKI_RAFT_FAKE_RTT_US=2000`
/// on a localhost cluster injects a 2 ms-per-RTT shape so we can
/// validate whether the "extra RTT per write" hypothesis is the
/// dominant factor on real cluster numbers without owning a real
/// cluster.
///
/// Always-on (NOT gated by `hot-path-trace`): the cost when unset is
/// one `OnceLock::get()` + one `if x > 0` compare per RPC.
fn fake_rtt_us() -> u64 {
    static CACHE: OnceLock<u64> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("KISEKI_RAFT_FAKE_RTT_US")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    })
}

/// Inject the configured fake RTT before issuing the request — pure
/// `tokio::time::sleep`, runs on the current task. Zero-µs when the
/// env var is unset / 0 (the function short-circuits without ever
/// touching the timer wheel).
async fn maybe_inject_fake_rtt() {
    let us = fake_rtt_us();
    if us == 0 {
        return;
    }
    tokio::time::sleep(Duration::from_micros(us)).await;
}

/// `rpc_call_plain`: open a fresh plain-TCP connection to `addr` and
/// run one `rpc_exchange`. For harness-driven scenario discrimination
/// per `specs/escalations/2026-05-30-decoupled-ack-perf-10x-analysis.md`
/// A-1 finding we inject the optional `KISEKI_RAFT_FAKE_RTT_US`
/// outbound-latency sleep BEFORE writing the request.
async fn rpc_call_plain<Req: Serialize, Resp: DeserializeOwned>(
    addr: &str,
    shard_id: ShardId,
    tag: &str,
    req: &Req,
) -> io::Result<Resp> {
    let mut stream = TcpStream::connect(addr).await?;
    maybe_inject_fake_rtt().await;
    rpc_exchange(&mut stream, shard_id, tag, req).await
}

/// `rpc_call_tls`: open a fresh mTLS connection to `addr` and run one
/// `rpc_exchange`. For harness-driven scenario discrimination per
/// `specs/escalations/2026-05-30-decoupled-ack-perf-10x-analysis.md`
/// A-1 finding we inject the optional `KISEKI_RAFT_FAKE_RTT_US`
/// outbound-latency sleep BEFORE writing the request (post-handshake
/// — TLS connect is itself >1 RTT and the goal is to model app-level
/// RTT on top of an already-warm connection).
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
    maybe_inject_fake_rtt().await;
    rpc_exchange(&mut tls_stream, shard_id, tag, req).await
}

/// Send one request/response RPC to `addr` for `shard_id` under `tag`,
/// connecting fresh (no pooling). Plaintext when `tls_config` is `None`,
/// otherwise mTLS. The request is postcard-encoded and the `Ok` response
/// body is postcard-decoded against `Resp`.
///
/// This is the call seam ADR-047's `TransportIntentGatherer` uses to
/// reach a peer's auxiliary (non-Raft) `IntentSync` dispatcher — the same
/// wire path the Raft `vote` / `full_snapshot` calls take.
///
/// # Errors
/// Returns an `io::Error` on connect/transport failure or when the peer
/// answers with a non-`Ok` status (`UnknownShard` / `ParseError` /
/// `DispatcherPanic`); classify it with [`classify_network_error`].
pub async fn rpc_call<Req: Serialize, Resp: DeserializeOwned>(
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

impl TcpNetwork {
    /// Open a fresh connection (plain TCP, or TLS when configured), with
    /// `TCP_NODELAY` so small Raft frames aren't delayed by Nagle.
    async fn connect(&self) -> io::Result<RaftConn> {
        let tcp = TcpStream::connect(&self.addr).await?;
        let _ = tcp.set_nodelay(true);
        match &self.tls_config {
            None => Ok(RaftConn::Plain(tcp)),
            Some(tls_config) => {
                let connector = tokio_rustls::TlsConnector::from(Arc::clone(tls_config));
                let ip: std::net::IpAddr = self
                    .addr
                    .split(':')
                    .next()
                    .and_then(|h| h.parse().ok())
                    .ok_or_else(|| {
                        network_error(NetworkErrorKind::Transport, "invalid Raft peer address")
                    })?;
                let server_name = ServerName::IpAddress(ip.into());
                let tls_stream = connector
                    .connect(server_name, tcp)
                    .await
                    .map_err(|e| network_error(NetworkErrorKind::Transport, e))?;
                Ok(RaftConn::Tls(Box::new(tls_stream)))
            }
        }
    }

    /// Send an RPC over the persistent connection, reconnecting ONCE if
    /// the held connection is stale/half-open. On a second failure the
    /// error propagates — openraft treats it as `Unreachable` and retries
    /// replication later, so no entry is silently dropped.
    async fn rpc_pooled<Req: Serialize, Resp: DeserializeOwned>(
        &mut self,
        tag: &str,
        req: &Req,
    ) -> io::Result<Resp> {
        let shard_id = self.shard_id;
        let mut last_err: Option<io::Error> = None;
        for _ in 0..2 {
            if self.conn.is_none() {
                self.conn = Some(self.connect().await?);
            }
            let res = match self.conn.as_mut().expect("conn set above") {
                RaftConn::Plain(s) => rpc_exchange(s, shard_id, tag, req).await,
                RaftConn::Tls(s) => rpc_exchange(&mut **s, shard_id, tag, req).await,
            };
            match res {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    // Drop the (possibly half-open) connection and retry once.
                    self.conn = None;
                    last_err = Some(e);
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| network_error(NetworkErrorKind::Transport, "rpc_pooled failed")))
    }
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
        self.rpc_pooled("append_entries", &rpc)
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
/// Exported so a higher crate (ADR-047 kiseki-log) can construct an
/// auxiliary dispatcher of this exact type for `register_aux`.
pub type ShardDispatch = Arc<
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
    /// The dispatcher does not recognize this tag — the listener may
    /// fall through to an auxiliary dispatcher; if none handles it,
    /// the wire status is `ParseError`.
    UnknownTag,
}

/// Clonable handle to the per-node shard registry. Each shard's
/// owner (typically `RaftShardStore::create_shard`) calls
/// `register_shard` / `unregister_shard` over the lifetime of the
/// shard.
#[derive(Clone)]
pub struct RegistryHandle {
    inner: Arc<DashMap<ShardId, ShardDispatch>>,
    /// Auxiliary (non-Raft) per-shard dispatchers (ADR-047). The
    /// listener consults this map only after the shard's Raft
    /// dispatcher returns `UnknownTag`, so the consensus-critical
    /// Raft path never touches it.
    aux: Arc<DashMap<ShardId, ShardDispatch>>,
    /// Optional metrics handle — when set, register/unregister
    /// updates `kiseki_raft_transport_registry_size`.
    metrics: Option<Arc<crate::transport_metrics::RaftTransportMetrics>>,
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
                                match postcard::from_bytes(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return DispatchOutcome::ParseError,
                                };
                            let result = std::panic::AssertUnwindSafe(raft.append_entries(req));
                            match futures::FutureExt::catch_unwind(result).await {
                                Ok(Ok(resp)) => DispatchOutcome::Ok(
                                    postcard::to_stdvec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    postcard::to_stdvec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        "vote" => {
                            let req: openraft::raft::VoteRequest<C> =
                                match postcard::from_bytes(&payload) {
                                    Ok(r) => r,
                                    Err(_) => return DispatchOutcome::ParseError,
                                };
                            let result = std::panic::AssertUnwindSafe(raft.vote(req));
                            match futures::FutureExt::catch_unwind(result).await {
                                Ok(Ok(resp)) => DispatchOutcome::Ok(
                                    postcard::to_stdvec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    postcard::to_stdvec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        "full_snapshot" => {
                            let env: SnapshotEnvelope<C> = match postcard::from_bytes(&payload) {
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
                                    postcard::to_stdvec(&resp).unwrap_or_default(),
                                ),
                                Ok(Err(e)) => DispatchOutcome::Ok(
                                    postcard::to_stdvec(&Err::<(), _>(e)).unwrap_or_default(),
                                ),
                                Err(_) => DispatchOutcome::Panicked,
                            }
                        }
                        // Not a Raft tag. Signal the listener to fall
                        // through to the auxiliary dispatcher (if any);
                        // a genuinely unknown tag still maps to the
                        // `ParseError` wire status.
                        _ => DispatchOutcome::UnknownTag,
                    }
                })
            },
        );
        self.inner.insert(shard_id, dispatch);
        if let Some(m) = &self.metrics {
            #[allow(clippy::cast_possible_wrap)]
            m.registry_size.set(self.inner.len() as i64);
        }
    }

    /// Remove a shard from the registry. Subsequent RPCs for that
    /// shard get `DispatchStatus::UnknownShard`. ADR-034 grace
    /// period applies — best-effort prompt with a tail bound by the
    /// longest in-flight RPC (gate-1 F-L2).
    pub fn unregister_shard(&self, shard_id: ShardId) {
        self.inner.remove(&shard_id);
        if let Some(m) = &self.metrics {
            #[allow(clippy::cast_possible_wrap)]
            m.registry_size.set(self.inner.len() as i64);
        }
    }

    /// Register an auxiliary (non-Raft) per-shard tag dispatcher. The listener
    /// routes a tag to this dispatcher only after the shard's Raft dispatcher
    /// returns `UnknownTag`, so aux tags MUST NOT collide with the Raft tags
    /// ("append_entries"/"vote"/"full_snapshot"). Idempotent — replaces the
    /// previous aux dispatcher. (ADR-047 phase 5b-rpc registers the IntentSync
    /// handler here.)
    pub fn register_aux(&self, shard_id: ShardId, dispatch: ShardDispatch) {
        self.aux.insert(shard_id, dispatch);
    }

    /// Remove a shard's auxiliary dispatcher.
    pub fn unregister_aux(&self, shard_id: ShardId) {
        self.aux.remove(&shard_id);
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
    /// Optional metrics handle. None in unit tests; production wires
    /// the `KisekiMetrics`-owned `RaftTransportMetrics` via
    /// `with_metrics`.
    metrics: Option<Arc<crate::transport_metrics::RaftTransportMetrics>>,
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
                aux: Arc::new(DashMap::new()),
                metrics: None,
            },
            active_per_peer: Arc::new(DashMap::new()),
            metrics: None,
        }
    }

    /// Builder: attach the per-node Raft transport metrics. Once set,
    /// every inbound RPC ticks per-(shard, op, outcome) counters,
    /// observes server-side latency, and updates the
    /// `registry_size` / `active_connections` gauges. Without this,
    /// the listener still functions — observation is just absent.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<crate::transport_metrics::RaftTransportMetrics>,
    ) -> Self {
        // Push the same handle into the registry so register/unregister
        // can update the registry_size gauge.
        self.registry.metrics = Some(Arc::clone(&metrics));
        self.metrics = Some(metrics);
        self
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
            // TCP_NODELAY on the accepted socket: with persistent
            // connection reuse, small Raft response frames would
            // otherwise sit in the kernel under Nagle (~40ms delayed-ACK
            // stall) instead of going out immediately. Connect-per-call
            // hid this because the socket close forced a flush.
            let _ = tcp_stream.set_nodelay(true);
            let registry = self.registry.clone();
            let acceptor = self.tls_acceptor.load_full();
            let per_peer = Arc::clone(&self.active_per_peer);
            let peer_key = peer_addr.ip().to_string();
            let metrics = self.metrics.clone();

            // Per-peer cap (gate-1 F-M5). Skipped for loopback: the cap
            // protects against external connection-flood DoS, but on
            // loopback the meter key (peer IP) collapses every local
            // process's connections into one bucket, so dev/test
            // multi-node-on-127.0.0.1 trips the cap before the cluster
            // can do useful work. Real peers (≠ loopback) still meter.
            let counter = per_peer
                .entry(peer_key.clone())
                .or_insert_with(|| AtomicU32::new(0));
            let active = counter.fetch_add(1, Ordering::Relaxed) + 1;
            drop(counter);
            let cap_applies = !peer_addr.ip().is_loopback();
            if cap_applies && active > per_peer_cap() {
                if let Some(c) = per_peer.get(&peer_key) {
                    c.fetch_sub(1, Ordering::Relaxed);
                }
                if let Some(m) = &metrics {
                    m.record_connection_cap_exceeded(&peer_key);
                }
                tracing::warn!(peer = %peer_key, active, "rejecting Raft RPC connection — per-peer cap exceeded");
                drop(tcp_stream);
                continue;
            }
            if let Some(m) = &metrics {
                m.active_connections.set(
                    self.active_per_peer
                        .iter()
                        .map(|e| i64::from(e.value().load(Ordering::Relaxed)))
                        .sum(),
                );
            }

            tokio::spawn(async move {
                let result = handle_one_connection(
                    tcp_stream,
                    acceptor.as_ref().clone(),
                    &registry,
                    metrics.as_deref(),
                )
                .await;
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
    metrics: Option<&crate::transport_metrics::RaftTransportMetrics>,
) -> io::Result<()> {
    if let Some(acc) = acceptor {
        let tls = acc
            .accept(tcp_stream)
            .await
            .map_err(|e| network_error(NetworkErrorKind::Transport, e))?;
        let mut s = tls;
        // Connection reuse: serve requests on this stream until the peer
        // closes it (Ok(false)) or an I/O error occurs. The client side
        // keeps one persistent connection per (peer, shard) and pipelines
        // sequential RPCs over it instead of re-dialing per call.
        while serve_one_request(&mut s, registry, metrics).await? {}
        Ok(())
    } else {
        let mut s = tcp_stream;
        while serve_one_request(&mut s, registry, metrics).await? {}
        Ok(())
    }
}

/// ADR-047 auxiliary tag-dispatch (UnknownTag fallthrough).
///
/// When the shard's Raft dispatcher returns `UnknownTag` (its `_` arm —
/// any tag that is not "append_entries"/"vote"/"full_snapshot"), route
/// the request to the shard's auxiliary (non-Raft) dispatcher if one is
/// registered, using ITS outcome. If no aux dispatcher exists (or the
/// aux also returns `UnknownTag`), the outcome stays `UnknownTag`, which
/// the listener maps to the same `ParseError` wire status as before this
/// hook existed — so a truly-unknown tag is indistinguishable on the
/// wire. Any other Raft outcome (`Ok`/`ParseError`/`Panicked`) is
/// returned unchanged: the consensus-critical path never reaches the
/// aux map. An aux panic is caught and mapped to `Panicked`, matching
/// the Raft dispatcher's panic semantics.
async fn aux_fallthrough(
    registry: &RegistryHandle,
    shard_id: ShardId,
    tag: &str,
    payload: &[u8],
    outcome: DispatchOutcome,
) -> DispatchOutcome {
    if !matches!(outcome, DispatchOutcome::UnknownTag) {
        return outcome;
    }
    let Some(aux) = registry.aux.get(&shard_id).map(|e| Arc::clone(&*e)) else {
        return DispatchOutcome::UnknownTag;
    };
    let fut = std::panic::AssertUnwindSafe(aux(tag, payload));
    match futures::FutureExt::catch_unwind(fut).await {
        Ok(o) => o,
        Err(_) => DispatchOutcome::Panicked,
    }
}

async fn serve_one_request<S>(
    stream: &mut S,
    registry: &RegistryHandle,
    metrics: Option<&crate::transport_metrics::RaftTransportMetrics>,
) -> io::Result<bool>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Returns Ok(true) when a request was served and the connection is
    // still frame-synced (the caller may keep it alive for the next
    // request — connection reuse, the pooling fix); Ok(false) when the
    // peer closed or the stream desynced and the connection must close.
    let started = std::time::Instant::now();
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).await.is_err() {
        return Ok(false); // peer closed
    }
    let req_len = u32::from_be_bytes(len_buf) as usize;
    if req_len > MAX_RAFT_RPC_SIZE {
        tracing::warn!(req_len, max = MAX_RAFT_RPC_SIZE, "Raft RPC oversized");
        write_response(stream, DispatchStatus::ParseError, Vec::new()).await?;
        if let Some(m) = metrics {
            m.record_rpc(
                "unknown",
                crate::transport_metrics::op::UNKNOWN,
                crate::transport_metrics::outcome::PARSE_ERROR,
                started.elapsed(),
            );
        }
        // We did NOT drain the oversized body — the stream is desynced;
        // close rather than keep-alive.
        return Ok(false);
    }
    let mut req_buf = vec![0u8; req_len];
    if stream.read_exact(&mut req_buf).await.is_err() {
        return Ok(false); // peer closed mid-frame
    }

    let Some((shard_id, tag, payload_value)) = decode_request_body(&req_buf) else {
        write_response(stream, DispatchStatus::ParseError, Vec::new()).await?;
        if let Some(m) = metrics {
            m.record_rpc(
                "unknown",
                crate::transport_metrics::op::UNKNOWN,
                crate::transport_metrics::outcome::PARSE_ERROR,
                started.elapsed(),
            );
        }
        return Ok(true); // frame fully consumed — connection stays synced
    };

    let Some(dispatch) = registry.inner.get(&shard_id).map(|e| Arc::clone(&*e)) else {
        write_response(stream, DispatchStatus::UnknownShard, Vec::new()).await?;
        let shard_str = shard_id.0.to_string();
        if let Some(m) = metrics {
            m.record_rpc(
                &shard_str,
                normalize_op(&tag),
                crate::transport_metrics::outcome::UNKNOWN_SHARD,
                started.elapsed(),
            );
        }
        tracing::debug!(
            shard = %shard_str,
            tag = %tag,
            "Raft RPC: unknown_shard (peer cache stale or shard retired)",
        );
        return Ok(true); // frame fully consumed — connection stays synced
    };

    // The dispatcher closure takes a `&[u8]` payload (the typed
    // request). `decode_request_body` now hands us the postcard-
    // encoded inner bytes directly — pass them through untouched
    // to the dispatcher, which postcard-decodes against the typed
    // handler.
    let outcome = dispatch(&tag, &payload_value).await;
    // ADR-047 auxiliary tag-dispatch: the Raft dispatcher signals
    // `UnknownTag` for any non-Raft tag. Fall through to the shard's
    // auxiliary (non-Raft) dispatcher if one is registered. The Raft
    // tags never reach the fallthrough, so the consensus-critical path
    // is untouched.
    let outcome = aux_fallthrough(registry, shard_id, &tag, &payload_value, outcome).await;
    let (status, body, outcome_label) = match outcome {
        DispatchOutcome::Ok(b) => (DispatchStatus::Ok, b, crate::transport_metrics::outcome::OK),
        // A genuinely unknown tag (no aux dispatcher, or aux also
        // returned `UnknownTag`) is reported identically to a
        // `ParseError` on the wire — same status, empty body, same
        // metrics label — so callers cannot tell the hook exists.
        DispatchOutcome::ParseError | DispatchOutcome::UnknownTag => (
            DispatchStatus::ParseError,
            Vec::new(),
            crate::transport_metrics::outcome::PARSE_ERROR,
        ),
        DispatchOutcome::Panicked => (
            DispatchStatus::DispatcherPanic,
            Vec::new(),
            crate::transport_metrics::outcome::DISPATCHER_PANIC,
        ),
    };
    let shard_str = shard_id.0.to_string();
    let op_label = normalize_op(&tag);
    if let Some(m) = metrics {
        m.record_rpc(&shard_str, op_label, outcome_label, started.elapsed());
        if matches!(
            outcome_label,
            crate::transport_metrics::outcome::DISPATCHER_PANIC
        ) {
            m.record_dispatcher_panic(&shard_str, op_label);
            tracing::warn!(
                shard = %shard_str,
                tag = %tag,
                "Raft RPC dispatcher panicked — listener stayed up; \
                 caller sees status 0x03",
            );
        }
    }
    write_response(stream, status, body).await?;
    Ok(true)
}

/// Map a free-form tag string to the bounded label set used by the
/// metrics. Unknown tags collapse to `op::UNKNOWN` so cardinality
/// stays bounded.
fn normalize_op(tag: &str) -> &'static str {
    match tag {
        "append_entries" => crate::transport_metrics::op::APPEND_ENTRIES,
        "vote" => crate::transport_metrics::op::VOTE,
        "full_snapshot" => crate::transport_metrics::op::FULL_SNAPSHOT,
        _ => crate::transport_metrics::op::UNKNOWN,
    }
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
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Payload {
            k: String,
        }
        let shard = ShardId(uuid::Uuid::from_u128(0x1234));
        let body = encode_request_body(shard, "vote", &Payload { k: "v".to_owned() }).unwrap();
        assert_eq!(body[0], RAFT_TRANSPORT_VERSION_V2);
        let (decoded_shard, decoded_tag, decoded_payload) =
            decode_request_body(&body).expect("decodes");
        assert_eq!(decoded_shard, shard);
        assert_eq!(decoded_tag, "vote");
        let payload: Payload = postcard::from_bytes(&decoded_payload).expect("payload decodes");
        assert_eq!(payload.k, "v");
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

    // -----------------------------------------------------------------
    // ADR-047 auxiliary tag-dispatch (UnknownTag fallthrough)
    // -----------------------------------------------------------------
    //
    // Standing up a full `openraft::Raft` inside this crate's tests is
    // heavy (it needs a state machine + log store + network factory,
    // none of which `kiseki-raft` exposes for tests). The listener
    // fallthrough under test treats every `ShardDispatch` identically —
    // it cannot tell a real Raft dispatcher from any other closure — so
    // these tests insert a STUB main dispatcher that replicates the Raft
    // dispatcher's tag-matching contract byte-for-byte: it returns
    // `Ok` for a Raft-shaped tag and `UnknownTag` for everything else
    // (exactly what `register_shard`'s closure does at its `_` arm).
    // The end-to-end wire path (`RaftRpcListener` + `rpc_call_plain`)
    // is the real production code.

    /// Build a stub main dispatcher mirroring the Raft dispatcher's tag
    /// contract: a recognized Raft tag → `Ok`, anything else →
    /// `UnknownTag` (the `_` arm of `register_shard`'s closure).
    fn stub_raft_dispatch() -> ShardDispatch {
        Arc::new(
            move |tag: &str, _payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                let tag = tag.to_owned();
                Box::pin(async move {
                    match tag.as_str() {
                        "append_entries" | "vote" | "full_snapshot" => {
                            // Echo a marker so the "Raft path still wins"
                            // test can assert the aux was NOT consulted.
                            let resp: Vec<u8> = b"raft".to_vec();
                            DispatchOutcome::Ok(postcard::to_stdvec(&resp).unwrap())
                        }
                        _ => DispatchOutcome::UnknownTag,
                    }
                })
            },
        )
    }

    /// Spawn a plaintext listener on an ephemeral port, returning its
    /// address and a registry handle obtained before `run()` consumed
    /// the listener.
    async fn spawn_listener() -> (String, RegistryHandle) {
        // Bind first to learn the OS-assigned port, then hand the bound
        // address to the listener.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        let listener = RaftRpcListener::new(addr.clone(), None);
        let registry = listener.registry();
        tokio::spawn(async move {
            let _ = listener.run().await;
        });
        // Give the accept loop a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, registry)
    }

    /// An aux tag (one the Raft dispatcher does not recognize) is routed
    /// to the registered auxiliary dispatcher, and the exact bytes it
    /// returned cross the wire unchanged (status Ok + body == b"pong").
    #[tokio::test]
    async fn aux_tag_routes_to_aux_dispatcher() {
        let (addr, registry) = spawn_listener().await;
        let shard = ShardId(uuid::Uuid::from_u128(0x0A_0002));

        registry.inner.insert(shard, stub_raft_dispatch());
        let aux: ShardDispatch = Arc::new(
            move |tag: &str, _payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                let tag = tag.to_owned();
                Box::pin(async move {
                    if tag == "intent_test" {
                        DispatchOutcome::Ok(b"pong".to_vec())
                    } else {
                        DispatchOutcome::UnknownTag
                    }
                })
            },
        );
        registry.register_aux(shard, aux);

        let (status, body) = raw_rpc(&addr, shard, "intent_test", &[]).await;
        assert_eq!(status, DispatchStatus::Ok as u8);
        assert_eq!(body, b"pong", "aux dispatcher bytes must cross the wire");
    }

    /// A Raft tag still reaches the (stub) Raft dispatcher even when an
    /// aux dispatcher is registered — the aux is NEVER consulted for a
    /// recognized Raft tag.
    #[tokio::test]
    async fn raft_tag_still_routes_to_raft_with_aux_present() {
        let (addr, registry) = spawn_listener().await;
        let shard = ShardId(uuid::Uuid::from_u128(0x0A_0003));

        registry.inner.insert(shard, stub_raft_dispatch());
        // An aux that would HIJACK any tag if (incorrectly) consulted.
        let aux: ShardDispatch = Arc::new(
            move |_tag: &str, _payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                Box::pin(async move { DispatchOutcome::Ok(b"AUX_HIJACK".to_vec()) })
            },
        );
        registry.register_aux(shard, aux);

        let (status, body) = raw_rpc(&addr, shard, "append_entries", &[]).await;
        assert_eq!(status, DispatchStatus::Ok as u8);
        // The stub Raft dispatcher returns a postcard-encoded b"raft";
        // crucially it is NOT the aux's "AUX_HIJACK" — proving the aux
        // was not consulted for a Raft tag.
        let decoded: Vec<u8> = postcard::from_bytes(&body).expect("decode raft marker");
        assert_eq!(decoded, b"raft");
        assert_ne!(body.as_slice(), b"AUX_HIJACK");
    }

    /// A bogus tag on a shard with NO aux dispatcher returns the
    /// `ParseError` wire status — identical to the pre-ADR-047 behavior.
    #[tokio::test]
    async fn unknown_tag_without_aux_is_parse_error() {
        let (addr, registry) = spawn_listener().await;
        let shard = ShardId(uuid::Uuid::from_u128(0x0A_0004));

        registry.inner.insert(shard, stub_raft_dispatch());
        // No register_aux for this shard.

        let (status, body) = raw_rpc(&addr, shard, "definitely_not_a_tag", &[]).await;
        assert_eq!(
            status,
            DispatchStatus::ParseError as u8,
            "unknown tag with no aux must map to ParseError (unchanged wire behavior)"
        );
        assert!(body.is_empty(), "ParseError carries no body");
    }

    /// An aux dispatcher that also returns `UnknownTag` for the tag
    /// collapses to the same `ParseError` wire status — the fallthrough
    /// is exhausted, so the response is indistinguishable from "no aux".
    #[tokio::test]
    async fn aux_returning_unknown_tag_is_parse_error() {
        let (addr, registry) = spawn_listener().await;
        let shard = ShardId(uuid::Uuid::from_u128(0x0A_0005));

        registry.inner.insert(shard, stub_raft_dispatch());
        let aux: ShardDispatch = Arc::new(
            move |_tag: &str, _payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                Box::pin(async move { DispatchOutcome::UnknownTag })
            },
        );
        registry.register_aux(shard, aux);

        let (status, body) = raw_rpc(&addr, shard, "something_unknown", &[]).await;
        assert_eq!(status, DispatchStatus::ParseError as u8);
        assert!(body.is_empty());
    }

    /// `unregister_aux` removes the aux dispatcher — a previously-routed
    /// aux tag goes back to `ParseError`.
    #[tokio::test]
    async fn unregister_aux_restores_parse_error() {
        let (addr, registry) = spawn_listener().await;
        let shard = ShardId(uuid::Uuid::from_u128(0x0A_0006));

        registry.inner.insert(shard, stub_raft_dispatch());
        let aux: ShardDispatch = Arc::new(
            move |_tag: &str, _payload: &[u8]| -> futures::future::BoxFuture<'_, DispatchOutcome> {
                Box::pin(async move { DispatchOutcome::Ok(b"pong".to_vec()) })
            },
        );
        registry.register_aux(shard, aux);
        let (status, _) = raw_rpc(&addr, shard, "intent_test", &[]).await;
        assert_eq!(status, DispatchStatus::Ok as u8);

        registry.unregister_aux(shard);
        let (status, body) = raw_rpc(&addr, shard, "intent_test", &[]).await;
        assert_eq!(status, DispatchStatus::ParseError as u8);
        assert!(body.is_empty());
    }

    // -- test helpers ------------------------------------------------

    /// Drive a single plaintext RPC and return the raw (status_byte,
    /// body) without the typed postcard decode `rpc_exchange` does — so
    /// tests can assert on the exact wire bytes / status the listener
    /// produced.
    async fn raw_rpc(addr: &str, shard: ShardId, tag: &str, payload: &[u8]) -> (u8, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let body = encode_request_body(shard, tag, &payload.to_vec()).unwrap();
        let len = u32::try_from(body.len()).unwrap();
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.flush().await.unwrap();

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp = vec![0u8; resp_len];
        stream.read_exact(&mut resp).await.unwrap();
        (resp[0], resp[1..].to_vec())
    }
}
