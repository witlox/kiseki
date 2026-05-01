//! World sub-structs — decompose the monolithic `KisekiWorld` into
//! focused groups. Each group owns the fields and Default for its
//! domain. `KisekiWorld` composes them.
//!
//! Groups:
//! - `legacy`: In-memory domain objects (@unit steps — will shrink as
//!   steps migrate to server harness)
//! - `server`: Running kiseki-server + network clients (@integration)
//! - `control`: Control-plane state (ADR-027)
//! - `small_file`: Small-file placement state (ADR-030)
//! - `block`: Block storage device state (ADR-029)
//! - `pnfs`: pNFS Flexible Files state (ADR-038)
//! - `backup`: Backup/restore state (ADR-016)
//! - `raft`: Raft cluster + perf state (ADR-037)
//! - `kms`: External KMS state (ADR-028)

pub mod backup;
pub mod block;
pub mod control;
pub mod kms;
pub mod legacy;
pub mod pnfs;
pub mod raft;
pub mod small_file;
