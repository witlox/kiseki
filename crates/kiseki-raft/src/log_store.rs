//! Generic in-memory Raft log store.
//!
//! Reusable across all Raft groups (key manager, log shards, audit).
//! Pattern follows `openraft/examples/log-mem/src/log_store.rs`.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::alias::{LogIdOf, VoteOf};
use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::{LogState, RaftTypeConfig};

#[derive(Debug)]
struct Inner<C: RaftTypeConfig> {
    last_purged_log_id: Option<LogIdOf<C>>,
    log: BTreeMap<u64, C::Entry>,
    committed: Option<LogIdOf<C>>,
    vote: Option<VoteOf<C>>,
}

impl<C: RaftTypeConfig> Default for Inner<C> {
    fn default() -> Self {
        Self {
            last_purged_log_id: None,
            log: BTreeMap::new(),
            committed: None,
            vote: None,
        }
    }
}

/// Generic in-memory Raft log store, parameterized over type config.
///
/// Use this for any Raft group that doesn't need persistent storage.
#[derive(Debug)]
pub struct MemLogStore<C: RaftTypeConfig> {
    inner: Arc<futures::lock::Mutex<Inner<C>>>,
}

impl<C: RaftTypeConfig> Clone for MemLogStore<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C: RaftTypeConfig> Default for MemLogStore<C> {
    fn default() -> Self {
        Self {
            inner: Arc::new(futures::lock::Mutex::new(Inner::default())),
        }
    }
}

impl<C: RaftTypeConfig> MemLogStore<C> {
    /// Create a new empty log store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<C: RaftTypeConfig> RaftLogReader<C> for MemLogStore<C>
where
    C::Entry: Clone,
{
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<C::Entry>, io::Error> {
        let inner = self.inner.lock().await;
        Ok(inner.log.range(range).map(|(_, v)| v.clone()).collect())
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        Ok(inner.vote.clone())
    }
}

impl<C: RaftTypeConfig> RaftLogStorage<C> for MemLogStore<C>
where
    C::Entry: Clone,
{
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let inner = self.inner.lock().await;
        let last = inner.log.iter().next_back().map(|(_, ent)| ent.log_id());
        let last_purged = inner.last_purged_log_id.clone();
        let last = match last {
            None => last_purged.clone(),
            Some(x) => Some(x),
        };
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        })
    }

    async fn save_committed(&mut self, committed: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        let mut inner = self.inner.lock().await;
        inner.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogIdOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        Ok(inner.committed.clone())
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        let mut inner = self.inner.lock().await;
        inner.vote = Some(vote.clone());
        Ok(())
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = C::Entry>,
    {
        let mut inner = self.inner.lock().await;
        for entry in entries {
            inner.log.insert(entry.index(), entry);
        }
        callback.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<C>>) -> Result<(), io::Error> {
        let mut inner = self.inner.lock().await;
        let start_index = match last_log_id {
            Some(ref log_id) => log_id.index() + 1,
            None => 0,
        };
        let keys: Vec<u64> = inner.log.range(start_index..).map(|(k, _)| *k).collect();
        for key in keys {
            inner.log.remove(&key);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogIdOf<C>) -> Result<(), io::Error> {
        let mut inner = self.inner.lock().await;
        let keys: Vec<u64> = inner
            .log
            .range(..=log_id.index())
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            inner.log.remove(&key);
        }
        inner.last_purged_log_id = Some(log_id);
        Ok(())
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }
}
