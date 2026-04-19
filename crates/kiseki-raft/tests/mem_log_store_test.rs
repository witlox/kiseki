//! Tests for the generic `MemLogStore`.

use std::io::Cursor;

use kiseki_raft::node::KisekiNode;
use kiseki_raft::MemLogStore;
use openraft::alias::{CommittedLeaderIdOf, LogIdOf};
use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::vote::RaftLeaderId;
use openraft::{EntryPayload, LogId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Req(String);
impl std::fmt::Display for Req {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resp;
impl std::fmt::Display for Resp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Resp")
    }
}

openraft::declare_raft_types!(
    pub TC:
        D = Req,
        R = Resp,
        NodeId = u64,
        Node = KisekiNode,
        SnapshotData = Cursor<Vec<u8>>,
);

type Store = MemLogStore<TC>;
type E = <TC as openraft::RaftTypeConfig>::Entry;

fn clid() -> CommittedLeaderIdOf<TC> {
    CommittedLeaderIdOf::<TC>::new(1, 1)
}

fn lid(index: u64) -> LogIdOf<TC> {
    LogId::new(clid(), index)
}

fn entry(index: u64, data: &str) -> E {
    RaftEntry::new(lid(index), EntryPayload::Normal(Req(data.into())))
}

fn noop() -> IOFlushed<TC> {
    IOFlushed::<TC>::noop()
}

#[tokio::test]
async fn empty_store() {
    let mut store = Store::new();
    let state = store.get_log_state().await.unwrap();
    assert!(state.last_purged_log_id.is_none());
    assert!(state.last_log_id.is_none());
}

#[tokio::test]
async fn append_and_read() {
    let mut store = Store::new();
    store
        .append(vec![entry(1, "a"), entry(2, "b")], noop())
        .await
        .unwrap();

    let entries = store.try_get_log_entries(1..=2).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].index(), 1);
    assert_eq!(entries[1].index(), 2);

    let state = store.get_log_state().await.unwrap();
    assert_eq!(state.last_log_id, Some(lid(2)));
}

#[tokio::test]
async fn vote_persistence() {
    let mut store = Store::new();
    assert!(store.read_vote().await.unwrap().is_none());

    let v = openraft::Vote::new(1, 1);
    store.save_vote(&v).await.unwrap();
    assert!(store.read_vote().await.unwrap().is_some());
}

#[tokio::test]
async fn truncate_removes_tail() {
    let mut store = Store::new();
    for i in 1..=5u64 {
        store
            .append(vec![entry(i, &format!("e{i}"))], noop())
            .await
            .unwrap();
    }
    store.truncate_after(Some(lid(3))).await.unwrap();
    let entries = store.try_get_log_entries(1..=5).await.unwrap();
    assert_eq!(entries.len(), 3);
}

#[tokio::test]
async fn purge_removes_head() {
    let mut store = Store::new();
    for i in 1..=5u64 {
        store
            .append(vec![entry(i, &format!("e{i}"))], noop())
            .await
            .unwrap();
    }
    store.purge(lid(3)).await.unwrap();
    let entries = store.try_get_log_entries(1..=5).await.unwrap();
    assert_eq!(entries.len(), 2);
    let state = store.get_log_state().await.unwrap();
    assert_eq!(state.last_purged_log_id, Some(lid(3)));
}

#[tokio::test]
async fn committed_persistence() {
    let mut store = Store::new();
    assert!(store.read_committed().await.unwrap().is_none());
    store.save_committed(Some(lid(42))).await.unwrap();
    assert_eq!(store.read_committed().await.unwrap(), Some(lid(42)));
}

#[tokio::test]
async fn clone_shares_state() {
    let mut store = Store::new();
    store
        .append(vec![entry(1, "shared")], noop())
        .await
        .unwrap();
    let mut reader = store.get_log_reader().await;
    let entries = reader.try_get_log_entries(1..=1).await.unwrap();
    assert_eq!(entries.len(), 1);
}
