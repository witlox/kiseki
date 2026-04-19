//! Stub network for single-node Raft.
//!
//! Implements sub-traits with unreachable errors (single-node never
//! replicates). Multi-node transport added in B.5.

use std::io;

use kiseki_raft::KisekiNode;
use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RaftNetworkFactory;

use super::types::KeyTypeConfig;

type C = KeyTypeConfig;

/// Network factory for single-node mode.
pub struct StubNetworkFactory;

/// Stub connection — never called in single-node mode.
pub struct StubNetwork;

impl RaftNetworkFactory<C> for StubNetworkFactory {
    type Network = StubNetwork;

    async fn new_client(&mut self, _target: u64, _node: &KisekiNode) -> Self::Network {
        StubNetwork
    }
}

impl RaftNetworkV2<C> for StubNetwork {
    async fn append_entries(
        &mut self,
        _rpc: openraft::raft::AppendEntriesRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::AppendEntriesResponse<C>, RPCError<C>> {
        Err(RPCError::Unreachable(Unreachable::new(&io::Error::new(
            io::ErrorKind::NotConnected,
            "single-node stub",
        ))))
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
        Err(openraft::error::StreamingError::Unreachable(
            Unreachable::new(&io::Error::new(
                io::ErrorKind::NotConnected,
                "single-node stub",
            )),
        ))
    }

    async fn vote(
        &mut self,
        _rpc: openraft::raft::VoteRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::VoteResponse<C>, RPCError<C>> {
        Err(RPCError::Unreachable(Unreachable::new(&io::Error::new(
            io::ErrorKind::NotConnected,
            "single-node stub",
        ))))
    }

    async fn transfer_leader(
        &mut self,
        _rpc: openraft::raft::TransferLeaderRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<(), RPCError<C>> {
        Err(RPCError::Unreachable(Unreachable::new(&io::Error::new(
            io::ErrorKind::NotConnected,
            "single-node stub",
        ))))
    }
}
