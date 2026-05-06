//! Microbench: single-thread Raft log append throughput.
//!
//! Run with `cargo test -p kiseki-raft --release --test raft_log_microbench
//! -- --nocapture --ignored`. Marked `#[ignore]` so it doesn't run in
//! the regular `cargo test` lane — the goal is steady-state numbers,
//! not pass/fail.
//!
//! Touches the same code path the openraft `RaftLogStorage::append`
//! impl drives: postcard encode + `FjallLogStore::append` (one
//! `WriteBatch::commit` with `PersistMode::SyncAll`).

use std::time::Instant;

use kiseki_raft::FjallLogStore;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FakeEntry {
    term: u64,
    index: u64,
    payload: Vec<u8>,
}

fn bench_append(entry_size: usize, count: u64, label: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FjallLogStore::open(&dir.path().join("log")).expect("open");

    let payload = vec![0xABu8; entry_size];
    let entry = FakeEntry {
        term: 1,
        index: 0,
        payload,
    };

    let start = Instant::now();
    for i in 1..=count {
        let mut e = entry.clone();
        e.index = i;
        store.append(i, &e).expect("append");
    }
    let elapsed = start.elapsed();
    let ops_per_sec = count as f64 / elapsed.as_secs_f64();
    println!(
        "{label}: count={count} entry_size={entry_size}B \
         elapsed={elapsed:?} throughput={ops_per_sec:>9.1} op/s"
    );
}

#[test]
#[ignore = "slow: microbench, run explicitly with --ignored"]
fn append_throughput_64b() {
    bench_append(64, 5_000, "append 64B");
}

#[test]
#[ignore = "slow: microbench, run explicitly with --ignored"]
fn append_throughput_512b() {
    bench_append(512, 5_000, "append 512B");
}

#[test]
#[ignore = "slow: microbench, run explicitly with --ignored"]
fn append_throughput_4kb() {
    bench_append(4_096, 5_000, "append 4KiB");
}
