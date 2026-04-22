//! Persistent Raft log store backed by redb.
//!
//! Wraps `RedbLogStore` and implements openraft's `RaftLogStorage` +
//! `RaftLogReader` traits. Raft state (log entries, vote, committed
//! index, last purged) survives server restart.
//!
//! Phase 12b: replaces `MemLogStore` for production deployments.

use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use openraft::alias::{LogIdOf, VoteOf};
use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::{LogState, RaftTypeConfig};
use serde::{de::DeserializeOwned, Serialize};

use crate::redb_log_store::RedbLogStore;

/// Persistent Raft log store backed by redb.
///
/// Stores log entries in the `raft_log` table and metadata (vote,
/// committed, last_purged) in the `raft_meta` table. Thread-safe
/// via `Arc` — `Clone` shares the underlying database.
#[derive(Clone)]
pub struct RedbRaftLogStore<C: RaftTypeConfig> {
    redb: Arc<RedbLogStore>,
    _phantom: std::marker::PhantomData<C>,
}

impl<C: RaftTypeConfig> RedbRaftLogStore<C> {
    /// Open or create a persistent Raft log store.
    pub fn open(path: &Path) -> io::Result<Self> {
        let redb = RedbLogStore::open(path)?;
        Ok(Self {
            redb: Arc::new(redb),
            _phantom: std::marker::PhantomData,
        })
    }

    /// Check whether this store has any persisted state (log entries or vote).
    ///
    /// Returns `true` if the store was previously used — the Raft node
    /// should NOT call `initialize()` on restart.
    pub fn has_state(&self) -> bool {
        self.redb.len().unwrap_or(0) > 0
            || self
                .redb
                .get_meta::<serde_json::Value>("vote")
                .ok()
                .flatten()
                .is_some()
    }
}

impl<C: RaftTypeConfig> RaftLogReader<C> for RedbRaftLogStore<C>
where
    C::Entry: DeserializeOwned + Clone,
    VoteOf<C>: DeserializeOwned,
{
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<C::Entry>, io::Error> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(&s) => s,
            std::ops::Bound::Excluded(&s) => s + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(&e) => e,
            std::ops::Bound::Excluded(&e) => e.saturating_sub(1),
            std::ops::Bound::Unbounded => u64::MAX,
        };
        let entries: Vec<(u64, C::Entry)> = self.redb.range(start, end)?;
        Ok(entries.into_iter().map(|(_, e)| e).collect())
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        self.redb.get_meta("vote")
    }
}

impl<C: RaftTypeConfig> RaftLogStorage<C> for RedbRaftLogStore<C>
where
    C::Entry: Serialize + DeserializeOwned + Clone,
    VoteOf<C>: Serialize + DeserializeOwned,
    LogIdOf<C>: Serialize + DeserializeOwned,
{
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let last_purged: Option<LogIdOf<C>> = self.redb.get_meta("last_purged")?;
        let last_index = self.redb.last_index()?;
        let last_log_id = if let Some(idx) = last_index {
            let entry: Option<C::Entry> = self.redb.get(idx)?;
            entry.map(|e| e.log_id())
        } else {
            last_purged.clone()
        };
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id,
        })
    }

    async fn save_committed(&mut self, committed: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        self.redb.set_meta("committed", &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogIdOf<C>>, io::Error> {
        self.redb
            .get_meta::<Option<LogIdOf<C>>>("committed")
            .map(Option::flatten)
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        self.redb.set_meta("vote", vote)
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = C::Entry>,
    {
        for entry in entries {
            let idx = entry.index();
            self.redb.append(idx, &entry)?;
        }
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        if let Some(ref log_id) = last_log_id {
            self.redb.truncate_after(log_id.index())?;
        } else {
            // Truncate everything — remove all entries.
            self.redb.truncate_before(u64::MAX)?;
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        // Remove entries up to and including log_id.index().
        self.redb.truncate_before(log_id.index() + 1)?;
        self.redb.set_meta("last_purged", &log_id)?;
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use a concrete type config for testing — borrow from kiseki-log's LogTypeConfig.
    // Since we can't depend on kiseki-log from kiseki-raft, define a minimal test config.
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct TestCmd(String);
    impl std::fmt::Display for TestCmd {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct TestResp;
    impl std::fmt::Display for TestResp {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "ok")
        }
    }

    openraft::declare_raft_types!(
        TestConfig:
            D = TestCmd,
            R = TestResp,
            NodeId = u64,
            Node = crate::node::KisekiNode,
            SnapshotData = std::io::Cursor<Vec<u8>>,
    );

    #[tokio::test]
    async fn vote_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vote.redb");

        // Write vote.
        {
            let mut store = RedbRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = openraft::Vote::new(1, 42);
            store.save_vote(&vote).await.unwrap();
        }

        // Reopen and read.
        {
            let mut store = RedbRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = store.read_vote().await.unwrap();
            assert!(vote.is_some());
        }
    }

    #[tokio::test]
    async fn has_state_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbRaftLogStore::<TestConfig>::open(&dir.path().join("empty.redb")).unwrap();
        assert!(!store.has_state());
    }

    #[tokio::test]
    async fn has_state_after_vote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voted.redb");
        let mut store = RedbRaftLogStore::<TestConfig>::open(&path).unwrap();
        let vote = openraft::Vote::new(1, 1);
        store.save_vote(&vote).await.unwrap();
        assert!(store.has_state());
    }
}
