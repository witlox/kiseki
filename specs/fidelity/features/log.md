# Fidelity: kiseki-log (log.feature)

## Scenario → Test Mapping

| # | Scenario | Test(s) | Depth |
|---|----------|---------|-------|
| 1 | Successful delta append | `successful_delta_append` | THOROUGH |
| 2 | Delta with inline data | `inline_data_delta` | THOROUGH |
| 3 | Deltas maintain total order | `total_order_within_shard` | THOROUGH |
| 4 | Raft leader loss triggers election | — | NONE (no Raft) |
| 5 | Write during leader election rejected | — | NONE (no Raft) |
| 6 | Quorum loss makes shard unavailable | — | NONE (no Raft) |
| 7 | Quorum recovery resumes | — | NONE (no Raft) |
| 8 | Shard split triggered by ceiling | `shard_split_redistributes_deltas` | MODERATE |
| 9 | Shard split does not block writes | — | NONE |
| 10 | Automatic compaction | `compaction_keeps_latest_per_key` | MODERATE |
| 11 | Admin-triggered compaction | — (same logic) | MODERATE |
| 12 | Delta GC respects watermarks | `gc_respects_consumer_watermarks` | THOROUGH |
| 13 | Stalled consumer blocks GC | `stalled_consumer_detected` (watermark) | MODERATE |
| 14 | Maintenance mode rejects writes | `maintenance_mode_rejects_writes` | THOROUGH |
| 15 | Exiting maintenance resumes | `exit_maintenance_resumes_writes` | THOROUGH |
| 16 | Stream processor reads range | `read_delta_range` | THOROUGH |
| 17 | Delta append during split | — | NONE |
| 18 | Concurrent split and compaction | — | NONE |
| 19-21 | Advisory scenarios | — | NONE (advisory integration) |

## Summary: 7/21 THOROUGH/MODERATE, 14/21 NONE (mostly Raft + advisory).

## Confidence: **LOW**

Core append/read/GC/compaction semantics are tested against the in-memory store. But Raft consensus (I-L1, I-L2), leader election, quorum handling, and split-under-load are all untested — these are the defining behaviors of the Log context.
