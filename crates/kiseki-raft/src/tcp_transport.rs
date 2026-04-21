//! TCP transport for multi-node Raft.
//!
//! Implements openraft's `RaftNetworkFactory` and `RaftNetworkV2` traits
//! over TCP with JSON serialization. Per ADR-026 (Strategy A).
//!
//! Each Raft RPC (`AppendEntries`, `Vote`, Snapshot) is serialized with
//! `serde_json`, length-prefixed (u32 big-endian), and sent over TCP.
//! MVP: plaintext TCP. Production requires mTLS (G-ADV-11).

use std::io;
use std::sync::Arc;

use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RaftNetworkFactory;
use openraft::RaftTypeConfig;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::node::KisekiNode;

/// TCP network factory — creates connections to Raft peers.
pub struct TcpNetworkFactory<C: RaftTypeConfig> {
    _phantom: std::marker::PhantomData<C>,
}

impl<C: RaftTypeConfig> TcpNetworkFactory<C> {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

/// A TCP connection to a single Raft peer.
pub struct TcpNetwork {
    addr: String,
}

impl<C: RaftTypeConfig<Node = KisekiNode>> RaftNetworkFactory<C> for TcpNetworkFactory<C> {
    type Network = TcpNetwork;

    async fn new_client(&mut self, _target: C::NodeId, node: &KisekiNode) -> TcpNetwork {
        TcpNetwork {
            addr: node.addr.clone(),
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

    // Read response.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;

    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    serde_json::from_slice(&resp_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn to_rpc_error<C: RaftTypeConfig>(e: io::Error) -> RPCError<C> {
    RPCError::Unreachable(Unreachable::new(&e))
}

impl<C: RaftTypeConfig> RaftNetworkV2<C> for TcpNetwork
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
        _vote: openraft::alias::VoteOf<C>,
        _snapshot: openraft::alias::SnapshotOf<C>,
        _cancel: impl futures::Future<Output = openraft::error::ReplicationClosed>
            + openraft::OptionalSend
            + 'static,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::SnapshotResponse<C>, openraft::error::StreamingError<C>> {
        // TODO: Implement snapshot streaming over TCP.
        // For now, return Unreachable (snapshot transfer deferred).
        Err(openraft::error::StreamingError::Unreachable(
            Unreachable::new(&io::Error::new(
                io::ErrorKind::NotConnected,
                "snapshot transfer not yet implemented",
            )),
        ))
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
pub async fn run_raft_rpc_server<C: RaftTypeConfig>(
    addr: &str,
    raft: Arc<openraft::Raft<C, impl openraft::storage::RaftStateMachine<C>>>,
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
            let mut req_buf = vec![0u8; req_len];
            if stream.read_exact(&mut req_buf).await.is_err() {
                return;
            }

            // Dispatch based on RPC type tag.
            // Client sends ("append_entries", payload) or ("vote", payload).
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
