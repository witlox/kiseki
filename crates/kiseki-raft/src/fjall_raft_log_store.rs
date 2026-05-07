//! Persistent Raft log store backed by fjall.
//!
//! Wraps [`FjallLogStore`] and implements openraft's `RaftLogStorage`
//! + `RaftLogReader` traits. Raft state (log entries, vote, committed
//! index, last purged) survives server restart.
//!
//! ADR-022 rev-2 successor: replaces the previous `RedbRaftLogStore`
//! 2026-05-06. Wire / serde-json format unchanged so a `git revert`
//! to the redb impl doesn't require re-encoding entries.

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

use crate::fjall_log_store::FjallLogStore;

/// Persistent Raft log store backed by fjall.
///
/// Stores log entries in the `raft_log` keyspace and metadata
/// (`vote`, `committed`, `last_purged`) in the `raft_meta` keyspace.
/// Thread-safe via `Arc` — `Clone` shares the underlying database.
#[derive(Clone)]
pub struct FjallRaftLogStore<C: RaftTypeConfig> {
    inner: Arc<FjallLogStore>,
    _phantom: std::marker::PhantomData<C>,
}

impl<C: RaftTypeConfig> FjallRaftLogStore<C> {
    /// Open or create a persistent Raft log store at `path`. The
    /// path is a directory (fjall layout); callers that previously
    /// passed a `*.redb` file path should pass a sibling directory
    /// name with no extension.
    pub fn open(path: &Path) -> io::Result<Self> {
        let inner = FjallLogStore::open(path)?;
        Ok(Self {
            inner: Arc::new(inner),
            _phantom: std::marker::PhantomData,
        })
    }

    /// Check whether this store has any persisted state (log entries
    /// or vote). Returns `true` if the store was previously used —
    /// the Raft node should NOT call `initialize()` on restart.
    pub fn has_state(&self) -> bool {
        !self.inner.is_empty().unwrap_or(true) || self.inner.meta_exists("vote").unwrap_or(false)
    }
}

impl<C: RaftTypeConfig> RaftLogReader<C> for FjallRaftLogStore<C>
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
        let entries: Vec<(u64, C::Entry)> = self.inner.range(start, end)?;
        Ok(entries.into_iter().map(|(_, e)| e).collect())
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        self.inner.get_meta("vote")
    }
}

impl<C: RaftTypeConfig> RaftLogStorage<C> for FjallRaftLogStore<C>
where
    C::Entry: Serialize + DeserializeOwned + Clone,
    VoteOf<C>: Serialize + DeserializeOwned,
    LogIdOf<C>: Serialize + DeserializeOwned,
{
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let last_purged: Option<LogIdOf<C>> = self.inner.get_meta("last_purged")?;
        let last_index = self.inner.last_index()?;
        let last_log_id = if let Some(idx) = last_index {
            let entry: Option<C::Entry> = self.inner.get(idx)?;
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
        self.inner.set_meta("committed", &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogIdOf<C>>, io::Error> {
        self.inner
            .get_meta::<Option<LogIdOf<C>>>("committed")
            .map(Option::flatten)
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        self.inner.set_meta("vote", vote)
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = C::Entry>,
    {
        // Each `append` call is one fsync at the inner layer
        // (`PersistMode::SyncAll` per entry). This matches the redb
        // impl's per-entry txn commit. A future optimization can
        // batch the iterator into one fjall WriteBatch + one fsync,
        // but that requires confirming openraft's expectations on
        // partial-failure semantics — the conservative port keeps
        // parity with the redb durability shape.
        for entry in entries {
            let idx = entry.index();
            self.inner.append(idx, &entry)?;
        }
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        if let Some(ref log_id) = last_log_id {
            self.inner.truncate_after(log_id.index())?;
        } else {
            // Truncate everything — remove all entries.
            self.inner.truncate_before(u64::MAX)?;
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        // Remove entries up to and including log_id.index().
        self.inner.truncate_before(log_id.index() + 1)?;
        self.inner.set_meta("last_purged", &log_id)?;
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = dir.path().join("vote");

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = openraft::Vote::new(1, 42);
            store.save_vote(&vote).await.unwrap();
        }

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = store.read_vote().await.unwrap();
            assert!(vote.is_some());
        }
    }

    #[tokio::test]
    async fn has_state_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallRaftLogStore::<TestConfig>::open(&dir.path().join("empty")).unwrap();
        assert!(!store.has_state());
    }

    #[tokio::test]
    async fn entries_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist");

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            let vote = openraft::Vote::new(1, 10);
            store.save_vote(&vote).await.unwrap();
            // Append entries via the underlying store directly —
            // creating proper Entry types requires internal openraft
            // constructors. We verify persistence at the inner layer.
            store.inner.append(1, &"entry-1").unwrap();
            store.inner.append(2, &"entry-2").unwrap();
            store.inner.append(3, &"entry-3").unwrap();
        }

        {
            let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
            assert!(store.has_state(), "store should have state after reopen");
            assert_eq!(
                store.inner.get::<String>(1).unwrap(),
                Some("entry-1".to_string())
            );
            assert_eq!(
                store.inner.get::<String>(2).unwrap(),
                Some("entry-2".to_string())
            );
            assert_eq!(
                store.inner.get::<String>(3).unwrap(),
                Some("entry-3".to_string())
            );
            let vote = store.read_vote().await.unwrap();
            assert!(vote.is_some(), "vote should survive reopen");
        }
    }

    #[tokio::test]
    async fn has_state_after_vote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voted");
        let mut store = FjallRaftLogStore::<TestConfig>::open(&path).unwrap();
        let vote = openraft::Vote::new(1, 1);
        store.save_vote(&vote).await.unwrap();
        assert!(store.has_state());
    }
}
