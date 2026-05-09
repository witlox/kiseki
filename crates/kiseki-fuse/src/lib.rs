//! Safe Rust wrapper over libfuse 3.x via [`kiseki_fuse_sys`].
//!
//! `kiseki-fuse` is the safe layer that converts libfuse's C
//! request/reply machinery into a sync, type-system-checked Rust
//! `Filesystem` trait that consumers (today: `kiseki-client`'s FUSE
//! daemon) implement once. It's the public Rust API for FUSE-related
//! work in the kiseki workspace.
//!
//! # Architecture (one paragraph)
//!
//! libfuse runs the FUSE session loop on a C thread. Each kernel
//! request is delivered to one of our `extern "C"` trampolines along
//! with an opaque `fuse_req_t` token. The trampoline builds a typed
//! Rust [`reply`] token wrapping that pointer, dispatches into
//! the user's [`Filesystem`] impl synchronously,
//! and the impl consumes the reply token to finalize the request — or
//! hands the token to the [`Bridge`] for async
//! finalization on a tokio task. The session loop runs on a dedicated
//! `std::thread` named `kiseki-fuse-session`. If the impl panics or
//! drops a reply token without consuming it, the wrapper replies
//! `EIO` to libfuse so the kernel never sees a leaked request slot.
//!
//! # Safety contract
//!
//! The safety properties are catalogued as **I-FUSE-1..I-FUSE-8** in
//! `specs/invariants.md` and are summarized here. Each has a test in
//! `tests/safety_contract.rs` (Acceptance criterion 8 of the libfuse
//! swap plan).
//!
//! - **I-FUSE-1 — consume-once reply tokens.** Every reply type
//!   ([`ReplyAttr`](reply::ReplyAttr), [`ReplyData`](reply::ReplyData),
//!   etc.) consumes itself on `.attr(...)` / `.data(...)` / `.ok()` /
//!   `.error(...)`. Two consumes do not compile (each consume takes
//!   `self` by value); one consume is the contract.
//!
//! - **I-FUSE-2 — drop-without-consume = `EIO` + counter.** If a Rust
//!   handler returns or panics without consuming its reply token,
//!   the token's `Drop` issues `fuse_reply_err(EIO)` to libfuse,
//!   increments the Prometheus counter
//!   `kiseki_fuse_drop_without_consume_total{op}`, and logs at
//!   WARN. Debug builds additionally panic to surface the bug
//!   during development.
//!
//! - **I-FUSE-3 — async oneshot bridge.** Reply tokens are `!Send`.
//!   A handler that wants to finalize on a tokio task uses
//!   [`Bridge::spawn`](bridge::Bridge::spawn), which converts the
//!   `!Send` reply into a `Send`-internal envelope and finalizes on
//!   the tokio runtime with a configurable timeout (default 30 s).
//!   Bypassing the bridge by `tokio::spawn`-ing a closure that
//!   captures a reply token does not compile; this is verified by
//!   `tests/safety_contract.rs::reply_is_not_send_compile_fail`.
//!
//! - **I-FUSE-4 — cancellation = `EINTR`; plaintext zeroized.**
//!   When the bridge's tokio task is cancelled (its future dropped
//!   before completion), the wrapper replies `EINTR` to libfuse,
//!   not the default `EIO`. Plaintext owned by the cancelled future
//!   uses [`ZeroOnCancel`] so the bytes are
//!   `zeroize::zeroize`'d when the future drops, regardless of which
//!   `await` point was cancelled.
//!
//! - **I-FUSE-5 — dedicated session thread.** The libfuse session
//!   loop runs on a `std::thread` named `kiseki-fuse-session`, NOT
//!   on `tokio::task::spawn_blocking`. Avoids competing with
//!   kiseki's other blocking work for tokio's blocking-pool slots.
//!
//! - **I-FUSE-6 — `Filesystem: Send + Sync + 'static`.** The trait
//!   is unconstrained beyond `Send + Sync`; tightness is on the
//!   reply tokens (per I-FUSE-1..3).
//!
//! - **I-FUSE-7 — `max_pending_ops` cap.** The wrapper enforces an
//!   in-flight bridge map size cap (default 1024). When hit, new
//!   FUSE requests are immediately replied with `EAGAIN`. Configurable
//!   via [`KisekiFuseConfig::max_pending_ops`](mount::KisekiFuseConfig).
//!
//! - **I-FUSE-8 — session-thread crash = abort or remount.** If the
//!   libfuse session thread exits unexpectedly, the wrapper detects
//!   the join handle and either aborts (default, fail-fast) or
//!   attempts a single auto-remount (opt-in via
//!   [`KisekiFuseConfig::auto_remount`](mount::KisekiFuseConfig)).
//!   On second crash falls back to abort.
//!
//! # See also
//!
//! - ADR-043 rev 4 (`specs/architecture/adr/043-system-library-ffi.md`)
//! - `specs/implementation/libfuse-swap.md` (rev 3)
//! - `specs/invariants.md` §"FUSE wrapper invariants"
//! - `specs/failure-modes.md` §"FUSE wrapper failures" (F-FUSE-1..3)
//! - `specs/escalations/2026-05-09-libfuse-syncfs-not-in-318-release.md`
//!   — `syncfs` lowlevel hook is not in libfuse 3.18.2; trait omits
//!   it pending architect resolution.

// FFI-shape lints relaxed crate-wide:
// - `cast_possible_truncation` / `cast_sign_loss` / `cast_possible_wrap`:
//   libfuse's C signatures use `usize`, `i64`, `c_int` etc.; bridging
//   to kiseki's `u64` / `u32` shapes triggers these at every site.
//   Same precedent as `kiseki-transport`.
// - `doc_markdown`: bare identifiers like `OsStr`, `SystemTime`,
//   `FUSE_SETATTR` come up too often to backtick individually.
// - `similar_names`: uid/gid binding pairs throughout trampolines.
// - `too_many_arguments`: getlk/setlk are inherently 9-arg from the
//   POSIX flock shape; no useful refactor.
// - `module_name_repetitions`: workspace-level allow already.
// - `must_use_candidate`: noisy on internal helpers.
// - `manual_let_else`: refactor pattern; not load-bearing.
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::similar_names)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::let_and_return)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::cast_lossless)]
// SAFETY: this crate wraps libfuse FFI. The unsafe code is confined to:
//
// 1. Building/destroying `fuse_session` and `fuse_args` (`session.rs`).
// 2. Calling `fuse_reply_*` finalizers (`reply.rs`). libfuse documents
//    these as thread-safe and idempotently consumes the request handle.
// 3. Reading `fuse_req_ctx` and `fuse_req_userdata` to populate
//    [`Request`](request::Request) and dispatch into the user's trait
//    (`trampolines.rs`). These calls are valid for the lifetime of the
//    `fuse_req_t` we received; libfuse owns the handle until we reply.
// 4. Building `extern "C" fn` trampolines that cast `*mut c_void`
//    userdata back to `&Arc<dyn Filesystem>` (`trampolines.rs`).
//    The userdata pointer is set at session creation and is valid
//    for the session's lifetime.
//
// Every `unsafe { ... }` block in this crate carries an inline `//
// SAFETY:` comment naming the libfuse documented invariant it relies
// on.
#![allow(unsafe_code)]

pub mod bridge;
pub mod error;
pub mod filesystem;
pub mod mount;
pub mod reply;
pub mod request;
pub mod session;
pub mod trampolines;
pub mod types;
pub mod zeroize;

pub use bridge::Bridge;
pub use error::FuseError;
pub use filesystem::Filesystem;
pub use mount::{mount, KisekiFuseConfig};
pub use request::{Request, RequestId};
pub use types::{FileAttr, FileType, LockOwner, SetAttrRequest, SetAttrValid};
pub use zeroize::ZeroOnCancel;
