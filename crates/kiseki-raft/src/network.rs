//! Generic stub network for single-node Raft.
//!
//! Reusable across all Raft groups. Returns `Unreachable` for all
//! RPCs — single-node Raft never replicates.

use std::io;

use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RaftNetworkFactory;
use openraft::RaftTypeConfig;

/// Generic network factory for single-node mode.
pub struct StubNetworkFactory<C: RaftTypeConfig>(std::marker::PhantomData<C>);

impl<C: RaftTypeConfig> Default for StubNetworkFactory<C> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<C: RaftTypeConfig> StubNetworkFactory<C> {
    /// Create a new stub factory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Stub connection — never called in single-node mode.
pub struct StubNetwork;

impl<C: RaftTypeConfig> RaftNetworkFactory<C> for StubNetworkFactory<C> {
    type Network = StubNetwork;

    async fn new_client(&mut self, _target: C::NodeId, _node: &C::Node) -> StubNetwork {
        StubNetwork
    }
}

fn unreachable_err<C: RaftTypeConfig>() -> RPCError<C> {
    RPCError::Unreachable(Unreachable::new(&io::Error::new(
        io::ErrorKind::NotConnected,
        "single-node stub",
    )))
}

impl<C: RaftTypeConfig> RaftNetworkV2<C> for StubNetwork {
    async fn append_entries(
        &mut self,
        _rpc: openraft::raft::AppendEntriesRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<openraft::raft::AppendEntriesResponse<C>, RPCError<C>> {
        Err(unreachable_err::<C>())
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
        Err(unreachable_err::<C>())
    }

    async fn transfer_leader(
        &mut self,
        _rpc: openraft::raft::TransferLeaderRequest<C>,
        _option: openraft::network::RPCOption,
    ) -> Result<(), RPCError<C>> {
        Err(unreachable_err::<C>())
    }
}
