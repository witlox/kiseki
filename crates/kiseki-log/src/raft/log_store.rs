//! In-memory Raft log storage for the Log shard.
//!
//! Pattern follows `openraft/examples/log-mem/src/log_store.rs`.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::alias::{LogIdOf, VoteOf};
use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::LogState;

use super::types::LogTypeConfig;

type C = LogTypeConfig;

#[derive(Debug, Default)]
struct Inner {
    last_purged_log_id: Option<LogIdOf<C>>,
    log: BTreeMap<u64, <C as openraft::RaftTypeConfig>::Entry>,
    committed: Option<LogIdOf<C>>,
    vote: Option<VoteOf<C>>,
}

/// In-memory Raft log store for the Log shard.
#[derive(Debug, Clone, Default)]
pub struct ShardLogStore {
    inner: Arc<futures::lock::Mutex<Inner>>,
}

impl ShardLogStore {
    /// Create a new empty log store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RaftLogReader<C> for ShardLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> Result<Vec<<C as openraft::RaftTypeConfig>::Entry>, io::Error> {
        let inner = self.inner.lock().await;
        Ok(inner.log.range(range).map(|(_, v)| v.clone()).collect())
    }

    async fn read_vote(&mut self) -> Result<Option<VoteOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        Ok(inner.vote)
    }
}

impl RaftLogStorage<C> for ShardLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<C>, io::Error> {
        let inner = self.inner.lock().await;
        let last = inner.log.iter().next_back().map(|(_, ent)| ent.log_id());
        let last_purged = inner.last_purged_log_id;
        let last = match last {
            None => last_purged,
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
        Ok(inner.committed)
    }

    async fn save_vote(&mut self, vote: &VoteOf<C>) -> Result<(), io::Error> {
        let mut inner = self.inner.lock().await;
        inner.vote = Some(*vote);
        Ok(())
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<C>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = <C as openraft::RaftTypeConfig>::Entry>,
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
            Some(log_id) => log_id.index() + 1,
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
