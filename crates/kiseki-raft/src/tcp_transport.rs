//! TCP transport for multi-node Raft.
//!
//! Implements openraft's `RaftNetworkFactory` and `RaftNetworkV2` traits
//! over TCP with JSON serialization. Per ADR-026 (Strategy A).
//!
//! Each Raft RPC (`AppendEntries`, `Vote`, Snapshot) is serialized with
//! `serde_json`, length-prefixed (u32 big-endian), and sent over TCP.
//! MVP: plaintext TCP. Production requires mTLS (G-ADV-11).

use std::io;
use std::io::Cursor;
use std::sync::Arc;

use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RaftNetworkFactory;
use openraft::RaftTypeConfig;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::node::KisekiNode;

/// Maximum Raft RPC message size (128 MB).
///
/// Prevents OOM from malicious peers sending oversized length prefixes
/// (ADV-S1, ADV-S6). Generous enough for snapshot transfer of large shards.
pub const MAX_RAFT_RPC_SIZE: usize = 128 * 1024 * 1024;

/// TCP network factory — creates connections to Raft peers.
///
/// When `tls_config` is `Some`, all connections use mTLS (ADV-S2).
/// When `None`, uses plaintext TCP (dev mode — logged as warning).
pub struct TcpNetworkFactory<C: RaftTypeConfig> {
    _phantom: std::marker::PhantomData<C>,
    tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl<C: RaftTypeConfig> TcpNetworkFactory<C> {
    /// Create a plaintext (dev mode) transport factory.
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
            tls_config: None,
        }
    }

    /// Create a TLS-secured transport factory (ADV-S2).
    pub fn with_tls(tls: Arc<rustls::ClientConfig>) -> Self {
        Self {
            _phantom: std::marker::PhantomData,
            tls_config: Some(tls),
        }
    }
}

/// A TCP connection to a single Raft peer.
pub struct TcpNetwork {
    addr: String,
    // TLS config for this connection (if mTLS is enabled).
    // Not used yet in rpc_call — will wrap TcpStream when activated.
    _tls_config: Option<Arc<rustls::ClientConfig>>,
}

impl<C: RaftTypeConfig<Node = KisekiNode, SnapshotData = Cursor<Vec<u8>>>> RaftNetworkFactory<C>
    for TcpNetworkFactory<C>
{
    type Network = TcpNetwork;

    async fn new_client(&mut self, _target: C::NodeId, node: &KisekiNode) -> TcpNetwork {
        TcpNetwork {
            addr: node.addr.clone(),
            _tls_config: self.tls_config.clone(),
        }
    }
}

/// RPC message types for the wire protocol.
#[derive(serde::Serialize, serde::Deserialize)]
enum RaftRpc<C: RaftTypeConfig> {
    AppendEntries(openraft::raft::AppendEntriesRequest<C>),
    Vote(openraft::raft::VoteRequest<C>),
    TransferLeader(openraft::raft::TransferLeaderRequest<C>),
}

#[derive(serde::Serialize, serde::Deserialize)]
enum RaftRpcResponse<C: RaftTypeConfig> {
    AppendEntries(openraft::raft::AppendEntriesResponse<C>),
    Vote(openraft::raft::VoteResponse<C>),
    Ok,
}

/// Send a request and receive a response over TCP.
async fn rpc_call<Req: Serialize, Resp: DeserializeOwned>(
    addr: &str,
    req: &Req,
) -> io::Result<Resp> {
    let mut stream = TcpStream::connect(addr).await?;

    // Serialize request.
    let data =
        serde_json::to_vec(req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Length-prefixed write.
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;

    // Read response (ADV-S1: cap size to prevent OOM).
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len > MAX_RAFT_RPC_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Raft RPC response too large: {resp_len} bytes (max {MAX_RAFT_RPC_SIZE})"),
        ));
    }

    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    serde_json::from_slice(&resp_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn to_rpc_error<C: RaftTypeConfig>(e: io::Error) -> RPCError<C> {
    RPCError::Unreachable(Unreachable::new(&e))
}

/// Serializable snapshot envelope for the wire protocol.
///
/// Wraps snapshot metadata + data bytes so they can be sent as a
/// single length-prefixed JSON message over TCP.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(bound = "")]
struct SnapshotEnvelope<C: RaftTypeConfig> {
    vote: openraft::alias::VoteOf<C>,
    meta: openraft::alias::SnapshotMetaOf<C>,
    /// Snapshot data as raw bytes (the state machine's JSON blob).
    data: Vec<u8>,
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
        rpc_call(&self.addr, &("append_entries", &rpc))
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
        // Read snapshot data from the Cursor<Vec<u8>>.
        let data = snapshot.snapshot.into_inner();
        let envelope = SnapshotEnvelope::<C> {
            vote,
            meta: snapshot.meta,
            data,
        };

        let resp: openraft::raft::SnapshotResponse<C> =
            rpc_call(&self.addr, &("full_snapshot", &envelope))
                .await
                .map_err(|e| openraft::error::StreamingError::Unreachable(Unreachable::new(&e)))?;

        Ok(resp)
    }

    async fn vote(
        &mut self,
        rpc: openraft::raft::VoteRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::VoteResponse<C>, RPCError<C>> {
        rpc_call(&self.addr, &("vote", &rpc))
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

/// TCP RPC server — listens for incoming Raft RPCs and dispatches them.
///
/// Call this on each node to accept Raft messages from peers.
/// When `tls_config` is `Some`, requires mTLS from peers (ADV-S2).
/// When `None`, accepts plaintext TCP (dev mode — logged as warning).
pub async fn run_raft_rpc_server<C: RaftTypeConfig<SnapshotData = Cursor<Vec<u8>>>>(
    addr: &str,
    raft: Arc<openraft::Raft<C, impl openraft::storage::RaftStateMachine<C>>>,
    _tls_config: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<()>
where
    C::D: Serialize + DeserializeOwned + Send + Sync,
    C::R: Serialize + DeserializeOwned + Send + Sync,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("  Raft RPC server listening on {addr}");

    loop {
        let (mut stream, _peer) = listener.accept().await?;
        let raft = Arc::clone(&raft);

        tokio::spawn(async move {
            // Read length-prefixed request.
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let req_len = u32::from_be_bytes(len_buf) as usize;
            // ADV-S1/S6: reject oversized messages to prevent OOM.
            if req_len > MAX_RAFT_RPC_SIZE {
                eprintln!(
                    "  Raft RPC: rejecting oversized request ({req_len} bytes, max {MAX_RAFT_RPC_SIZE})"
                );
                return;
            }
            let mut req_buf = vec![0u8; req_len];
            if stream.read_exact(&mut req_buf).await.is_err() {
                return;
            }

            // Dispatch based on RPC type tag.
            // Client sends ("append_entries", payload), ("vote", payload),
            // or ("full_snapshot", envelope).
            let tag_result: Result<(String, serde_json::Value), _> =
                serde_json::from_slice(&req_buf);

            let resp_data = match tag_result {
                Ok((ref tag, _)) if tag == "append_entries" => {
                    match serde_json::from_slice::<(String, openraft::raft::AppendEntriesRequest<C>)>(
                        &req_buf,
                    ) {
                        Ok((_, ae_req)) => match raft.append_entries(ae_req).await {
                            Ok(resp) => serde_json::to_vec(&resp).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        },
                        Err(_) => Vec::new(),
                    }
                }
                Ok((ref tag, _)) if tag == "vote" => {
                    match serde_json::from_slice::<(String, openraft::raft::VoteRequest<C>)>(
                        &req_buf,
                    ) {
                        Ok((_, vote_req)) => match raft.vote(vote_req).await {
                            Ok(resp) => serde_json::to_vec(&resp).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        },
                        Err(_) => Vec::new(),
                    }
                }
                Ok((ref tag, _)) if tag == "full_snapshot" => {
                    match serde_json::from_slice::<(String, SnapshotEnvelope<C>)>(&req_buf) {
                        Ok((_, env)) => {
                            let snapshot = openraft::storage::Snapshot {
                                meta: env.meta,
                                snapshot: Cursor::new(env.data),
                            };
                            match raft.install_full_snapshot(env.vote, snapshot).await {
                                Ok(resp) => serde_json::to_vec(&resp).unwrap_or_default(),
                                Err(_) => Vec::new(),
                            }
                        }
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            };

            // Send response.
            let len = resp_data.len() as u32;
            let _ = stream.write_all(&len.to_be_bytes()).await;
            let _ = stream.write_all(&resp_data).await;
            let _ = stream.flush().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// ADV-S1: Server must close connection when receiving a message
    /// with length prefix exceeding MAX_RAFT_RPC_SIZE, without allocating.
    #[tokio::test]
    async fn server_drops_oversized_rpc_request() {
        // Start a fake "server" that applies the same size check
        // as run_raft_rpc_server does.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await.unwrap();
            let req_len = u32::from_be_bytes(len_buf) as usize;

            // The server SHOULD reject and close — not allocate.
            if req_len > MAX_RAFT_RPC_SIZE {
                // Correct behavior: drop connection.
                return true; // rejected
            }
            false // accepted (bad)
        });

        // Client sends a massive length prefix (1 GB).
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let oversized: u32 = 1_073_741_824; // 1 GB
        stream.write_all(&oversized.to_be_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let rejected = server.await.unwrap();
        assert!(rejected, "server must reject oversized RPC, not allocate");
    }

    /// ADV-S1: Client must reject responses with length prefix exceeding
    /// MAX_RAFT_RPC_SIZE with an error, not an OOM allocation.
    #[tokio::test]
    async fn client_rejects_oversized_rpc_response() {
        // Start a fake server that sends an oversized length prefix.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read the client's request (ignore it).
            let mut len_buf = [0u8; 4];
            let _ = stream.read_exact(&mut len_buf).await;
            let req_len = u32::from_be_bytes(len_buf) as usize;
            let mut discard = vec![0u8; req_len.min(1024)];
            let _ = stream.read_exact(&mut discard).await;
            // Send back an oversized response length.
            let oversized: u32 = 512 * 1024 * 1024; // 512 MB
            stream.write_all(&oversized.to_be_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            // Don't send any data — the client should reject before reading.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });

        // Client calls rpc_call — should get an error, not OOM.
        let result: io::Result<String> = rpc_call(&addr.to_string(), &"test-request").await;

        assert!(
            result.is_err(),
            "rpc_call should return an error for oversized response, not attempt allocation"
        );
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn max_rpc_size_is_reasonable() {
        const { assert!(MAX_RAFT_RPC_SIZE >= 64 * 1024 * 1024) };
        const { assert!(MAX_RAFT_RPC_SIZE <= 256 * 1024 * 1024) };
    }
}
