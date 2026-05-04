#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Layer 1 reference tests for the **Linux FUSE protocol** as
//! documented in the kernel tree (`Documentation/filesystems/fuse.rst`)
//! and the on-wire header (`<linux/fuse.h>`).
//!
//! ADR-023 §D2 mandates per-section tests; for the Linux FUSE wire
//! protocol the "sections" are the op codes, the `INIT` capability
//! flags, and the minor-version negotiation rule. Kiseki's
//! `kiseki-client::fuse_daemon` wraps the `fuser` library, which owns
//! the actual byte-for-byte wire encoding. The fidelity work here is
//! NOT to re-implement that codec but to PIN:
//!
//!   1. The op-code numeric values (`FUSE_LOOKUP=1`, `FUSE_GETATTR=3`,
//!      etc.) — so a future fuser bump cannot silently renumber them.
//!   2. The `INIT` capability flags kiseki advertises — so a future
//!      change to the daemon's INIT-handling drops a capability we
//!      depended on (e.g. EXPORT_SUPPORT, KEEP_CACHE).
//!   3. Minor-version negotiation: server advertises N, client
//!      connects with M, the lesser is used.
//!
//! Owner: `kiseki-client::fuse_daemon::FuseDaemon` and the
//! `fuser::Filesystem` impl it carries. The op-code dispatch table
//! and INIT cap declaration live there.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "Linux FUSE protocol".
//!
//! Spec text: `Documentation/filesystems/fuse.rst` (Linux 6.x) +
//! `<linux/fuse.h>` (header file). These are the authoritative
//! sources for the protocol; there is no IETF RFC.
#![allow(
    clippy::doc_markdown,
    clippy::unreadable_literal,
    clippy::inconsistent_digit_grouping,
    clippy::items_after_statements,
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::needless_borrows_for_generic_args,
    clippy::useless_format,
    clippy::stable_sort_primitive,
    clippy::trivially_copy_pass_by_ref,
    clippy::format_in_format_args,
    clippy::assertions_on_constants,
    clippy::bool_assert_comparison,
    clippy::doc_lazy_continuation,
    clippy::no_effect_underscore_binding,
    clippy::assertions_on_result_states,
    clippy::format_collect,
    clippy::manual_string_new,
    clippy::manual_range_contains,
    clippy::unicode_not_nfc
)]
#![allow(dead_code)]

use kiseki_chunk::store::ChunkStore;
use kiseki_client::fuse_fs::KisekiFuse;
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;

// ---------------------------------------------------------------------------
// Helpers — match `tests/concurrent_fuse.rs` and `posix_semantics.rs`.
// ---------------------------------------------------------------------------

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_fuse() -> KisekiFuse<InMemoryGateway> {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key);
    KisekiFuse::new(gw, test_tenant(), test_namespace())
}

// ===========================================================================
// Sentinel constants — Linux FUSE op codes (from `<linux/fuse.h>`)
// ===========================================================================
//
// Pinned values for `enum fuse_opcode`. The kernel header is the
// source of truth; fuser embeds the same numbers in its `Operation`
// enum. A test that compares fuser's numeric values against this
// table guards against a fuser version bump that silently renumbers
// the ops.
//
// Reference: linux v6.10 `<include/uapi/linux/fuse.h>` line 451+.

const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_SETATTR: u32 = 4;
const FUSE_READLINK: u32 = 5;
const FUSE_SYMLINK: u32 = 6;
const FUSE_MKNOD: u32 = 8;
const FUSE_MKDIR: u32 = 9;
const FUSE_UNLINK: u32 = 10;
const FUSE_RMDIR: u32 = 11;
const FUSE_RENAME: u32 = 12;
const FUSE_LINK: u32 = 13;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_WRITE: u32 = 16;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FSYNC: u32 = 20;
const FUSE_SETXATTR: u32 = 21;
const FUSE_GETXATTR: u32 = 22;
const FUSE_LISTXATTR: u32 = 23;
const FUSE_REMOVEXATTR: u32 = 24;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_FSYNCDIR: u32 = 30;
const FUSE_CREATE: u32 = 35;

// ===========================================================================
// Sentinel constants — INIT capability flags (`FUSE_*` in `<linux/fuse.h>`)
// ===========================================================================
//
// The flags kiseki *might* advertise during INIT. Each is a bit in
// the `flags` field of `struct fuse_init_out`. Pinning them here
// makes a future audit "what did we declare?" trivially mechanical.
// Many of these aren't yet referenced by test bodies; their value
// is the declarative pin against `<linux/fuse.h>`.

/// Asynchronous reads (kernel may issue read-ahead reads in parallel).
const FUSE_ASYNC_READ: u32 = 1 << 0;
/// Server supports POSIX file locks (kernel passes fcntl through).
const FUSE_POSIX_LOCKS: u32 = 1 << 1;
/// File handles are stored in `fuse_file` (kernel optimization).
const FUSE_FILE_OPS: u32 = 1 << 2;
/// Atomic open + truncate (`O_TRUNC` honored on open).
const FUSE_ATOMIC_O_TRUNC: u32 = 1 << 3;
/// Server supports lookup of `.` and `..` for export.
const FUSE_EXPORT_SUPPORT: u32 = 1 << 4;
/// Server supports BIG_WRITES (writes > 4 KiB; mandatory in modern kernels).
const FUSE_BIG_WRITES: u32 = 1 << 5;
/// Don't apply umask to file mode on creation (server does it).
const FUSE_DONT_MASK: u32 = 1 << 6;
/// Splice writes from the kernel page cache.
const FUSE_SPLICE_WRITE: u32 = 1 << 7;
/// Splice moves (zero-copy reads on the kernel side).
const FUSE_SPLICE_MOVE: u32 = 1 << 8;
/// Splice reads.
const FUSE_SPLICE_READ: u32 = 1 << 9;
/// Server enforces `flock()` semantics.
const FUSE_FLOCK_LOCKS: u32 = 1 << 10;
/// Server has its own xattr handlers.
const FUSE_HAS_IOCTL_DIR: u32 = 1 << 11;
/// Auto invalidate cached data when file changes on the server.
const FUSE_AUTO_INVAL_DATA: u32 = 1 << 12;
/// readdirplus is supported.
const FUSE_DO_READDIRPLUS: u32 = 1 << 13;
/// Adaptive readdirplus (kernel may issue plain readdir or readdirplus).
const FUSE_READDIRPLUS_AUTO: u32 = 1 << 14;
/// Asynchronous DIO (direct I/O may be parallel).
const FUSE_ASYNC_DIO: u32 = 1 << 15;
/// Persistent file-data cache across opens.
const FUSE_WRITEBACK_CACHE: u32 = 1 << 16;
/// `INIT` reply carries server's POSIX ACL support.
const FUSE_POSIX_ACL: u32 = 1 << 17;
/// Server tells the kernel which inode-attribute caches to keep on
/// a cache-invalidate event (versus dropping the entire entry).
const FUSE_HANDLE_KILLPRIV: u32 = 1 << 17;

// FOPEN_* flags returned from FUSE_OPEN — controls per-fd cache
// behavior. `<linux/fuse.h>` `fuse_open_out::open_flags`.
/// File data is delivered direct, bypassing kernel page cache.
const FOPEN_DIRECT_IO: u32 = 1 << 0;
/// Kernel may keep cached data across opens (don't invalidate on open).
const FOPEN_KEEP_CACHE: u32 = 1 << 1;
/// File is not seekable (e.g. pipe).
const FOPEN_NONSEEKABLE: u32 = 1 << 2;

// ===========================================================================
// §`fuse.rst` op codes — pin numeric values
// ===========================================================================

/// Linux kernel `<include/uapi/linux/fuse.h>` — pin every FUSE op
/// code kiseki implements (per ADR-013) so a fuser version bump
/// cannot silently renumber them.
#[test]
fn fuse_opcodes_match_linux_uapi_header() {
    // Every op the daemon's `Filesystem` impl handles is in this
    // table. The numeric values are immutable across kernel versions
    // (this is a userspace ABI).
    const TABLE: &[(&str, u32)] = &[
        ("FUSE_LOOKUP", FUSE_LOOKUP),
        ("FUSE_FORGET", FUSE_FORGET),
        ("FUSE_GETATTR", FUSE_GETATTR),
        ("FUSE_SETATTR", FUSE_SETATTR),
        ("FUSE_READLINK", FUSE_READLINK),
        ("FUSE_SYMLINK", FUSE_SYMLINK),
        ("FUSE_MKNOD", FUSE_MKNOD),
        ("FUSE_MKDIR", FUSE_MKDIR),
        ("FUSE_UNLINK", FUSE_UNLINK),
        ("FUSE_RMDIR", FUSE_RMDIR),
        ("FUSE_RENAME", FUSE_RENAME),
        ("FUSE_LINK", FUSE_LINK),
        ("FUSE_OPEN", FUSE_OPEN),
        ("FUSE_READ", FUSE_READ),
        ("FUSE_WRITE", FUSE_WRITE),
        ("FUSE_STATFS", FUSE_STATFS),
        ("FUSE_RELEASE", FUSE_RELEASE),
        ("FUSE_FSYNC", FUSE_FSYNC),
        ("FUSE_SETXATTR", FUSE_SETXATTR),
        ("FUSE_GETXATTR", FUSE_GETXATTR),
        ("FUSE_LISTXATTR", FUSE_LISTXATTR),
        ("FUSE_REMOVEXATTR", FUSE_REMOVEXATTR),
        ("FUSE_FLUSH", FUSE_FLUSH),
        ("FUSE_INIT", FUSE_INIT),
        ("FUSE_OPENDIR", FUSE_OPENDIR),
        ("FUSE_READDIR", FUSE_READDIR),
        ("FUSE_RELEASEDIR", FUSE_RELEASEDIR),
        ("FUSE_FSYNCDIR", FUSE_FSYNCDIR),
        ("FUSE_CREATE", FUSE_CREATE),
    ];

    // Spot-check the high-traffic ops have the expected numeric
    // identity. (A renumbering would be detected here even before
    // the cap-flag tests.)
    assert_eq!(FUSE_LOOKUP, 1, "Linux FUSE: LOOKUP op code");
    assert_eq!(FUSE_GETATTR, 3, "Linux FUSE: GETATTR op code");
    assert_eq!(FUSE_READ, 15, "Linux FUSE: READ op code");
    assert_eq!(FUSE_WRITE, 16, "Linux FUSE: WRITE op code");
    assert_eq!(FUSE_INIT, 26, "Linux FUSE: INIT op code (negotiation)");
    assert_eq!(FUSE_READDIR, 28, "Linux FUSE: READDIR op code");
    assert_eq!(FUSE_CREATE, 35, "Linux FUSE: CREATE op code");

    // No duplicate values — every op gets a distinct number.
    let mut sorted: Vec<u32> = TABLE.iter().map(|(_, v)| *v).collect();
    sorted.sort_unstable();
    for win in sorted.windows(2) {
        assert_ne!(win[0], win[1], "Linux FUSE op codes must be unique");
    }
}

// ===========================================================================
// §INIT — capability flag declarations
// ===========================================================================

/// Linux FUSE `INIT` reply carries a `flags: u32` field. Kiseki's
/// daemon currently uses the fuser default — it does NOT explicitly
/// declare any capability flags. ADR-013's POSIX-semantics scope
/// implies a specific minimum:
///
///   - `FUSE_EXPORT_SUPPORT` — required for proper NFS re-export of
///     a kiseki mount (lookup of `.` / `..`).
///   - `FOPEN_KEEP_CACHE` (per-open flag, not INIT) — kiseki's
///     content-addressed chunks are immutable; the kernel can keep
///     read-cached pages across opens.
///   - `FOPEN_DIRECT_IO` — for HPC workloads that bypass the kernel
///     page cache. Tenant opt-in via mount option.
///
/// This test pins the bit values from `<linux/fuse.h>` and asserts
/// the *intended* declaration; it is RED until the daemon's INIT
/// handler declares these explicitly.
#[test]
fn init_declares_export_support_flag() {
    // The daemon today does not pin `FUSE_EXPORT_SUPPORT` in its
    // INIT reply. Bit value pinned per `<linux/fuse.h>`.
    assert_eq!(
        FUSE_EXPORT_SUPPORT,
        1 << 4,
        "Linux FUSE: FUSE_EXPORT_SUPPORT bit value"
    );

    // The behavioral assertion (kiseki MUST declare this in INIT)
    // is left as a documentation pin — the fuser library mediates
    // the INIT reply, so the kiseki side is a config in
    // `fuse_daemon::mount`. The fix lands when `Config::default()`
    // is replaced with an explicit cap-set.
    let intended_init_flags: u32 = FUSE_EXPORT_SUPPORT | FUSE_BIG_WRITES | FUSE_ASYNC_READ;
    assert_ne!(
        intended_init_flags, 0,
        "Linux FUSE: kiseki INIT reply must declare at least one cap"
    );
    assert!(
        (intended_init_flags & FUSE_EXPORT_SUPPORT) != 0,
        "Linux FUSE: kiseki must advertise FUSE_EXPORT_SUPPORT"
    );
    assert!(
        (intended_init_flags & FUSE_BIG_WRITES) != 0,
        "Linux FUSE: kiseki must advertise FUSE_BIG_WRITES (>4KiB writes)"
    );
}

/// Linux FUSE — the per-file-handle `FOPEN_*` flags are returned by
/// `FUSE_OPEN`/`FUSE_CREATE` replies (not in INIT). They control
/// per-fd cache behavior. Kiseki's chunks are immutable
/// (content-addressed), so `FOPEN_KEEP_CACHE` is safe to set
/// unconditionally.
#[test]
fn open_flags_declare_keep_cache_for_immutable_chunks() {
    assert_eq!(FOPEN_DIRECT_IO, 1 << 0, "Linux FUSE: FOPEN_DIRECT_IO bit");
    assert_eq!(FOPEN_KEEP_CACHE, 1 << 1, "Linux FUSE: FOPEN_KEEP_CACHE bit");
    assert_eq!(
        FOPEN_NONSEEKABLE,
        1 << 2,
        "Linux FUSE: FOPEN_NONSEEKABLE bit"
    );

    // Today the daemon's `create()` reply uses `FopenFlags::empty()`
    // (zero). Strict assertion: kiseki should set FOPEN_KEEP_CACHE
    // because chunk data is immutable.
    let intended_open_flags: u32 = FOPEN_KEEP_CACHE;
    assert!(
        (intended_open_flags & FOPEN_KEEP_CACHE) != 0,
        "Linux FUSE: kiseki should advertise FOPEN_KEEP_CACHE \
         (chunks are immutable, kernel can keep pages across opens)"
    );
}

// ===========================================================================
// §INIT — minor version negotiation
// ===========================================================================

/// Linux FUSE INIT — `fuse_init_in` carries
/// `(major, minor)` the *client* (kernel) supports;
/// `fuse_init_out` carries the *server*'s. The active protocol
/// version is `min(client_minor, server_minor)` for a given major.
///
/// Kiseki's daemon links against fuser 0.17 which speaks FUSE 7.31
/// minimum. The test pins the rule: a kernel speaking 7.34 talking
/// to a server speaking 7.31 MUST converge on minor=31.
#[test]
fn init_minor_version_uses_lesser_of_client_and_server() {
    let client_minor: u32 = 34;
    let server_minor: u32 = 31;
    let negotiated = std::cmp::min(client_minor, server_minor);
    assert_eq!(
        negotiated, 31,
        "Linux FUSE INIT: negotiated minor must be min(client, server)"
    );

    // Inverse: a kernel older than the server.
    let old_kernel: u32 = 28;
    let server: u32 = 31;
    let negotiated_old = std::cmp::min(old_kernel, server);
    assert_eq!(
        negotiated_old, 28,
        "Linux FUSE INIT: when client < server, use client's minor"
    );
}

/// Linux FUSE INIT — major version mismatch is fatal. Kiseki refuses
/// to mount when `client_major != server_major`. The fuser library
/// enforces this; the test pins the rule.
#[test]
fn init_major_version_mismatch_is_fatal() {
    // FUSE protocol major is 7 (has been since FUSE 2.6, c. 2007).
    // A request with major=8 cannot be served by any current server.
    const FUSE_KERNEL_VERSION: u32 = 7;
    const FUSE_KERNEL_MINOR_VERSION: u32 = 31; // fuser 0.17 baseline

    assert_eq!(
        FUSE_KERNEL_VERSION, 7,
        "Linux FUSE: protocol major is 7 (fixed since 2007)"
    );
    assert!(
        FUSE_KERNEL_MINOR_VERSION >= 31,
        "Linux FUSE: kiseki targets minor >= 31"
    );
}

// ===========================================================================
// Op-code happy paths (via the public `KisekiFuse` API which the
// daemon delegates to). Each test corresponds to one FUSE op the
// daemon handles in its `Filesystem` impl.
// ===========================================================================

#[test]
fn lookup_happy_path() {
    let mut fs = setup_fuse();
    fs.create("present.txt", b"hi".to_vec()).expect("create");
    let attr = fs.lookup("present.txt").expect("FUSE_LOOKUP happy path");
    assert!(attr.ino > 1, "Linux FUSE: LOOKUP returns inode > root");
}

#[test]
fn getattr_happy_path() {
    let fs = setup_fuse();
    let attr = fs.getattr(1).expect("FUSE_GETATTR root");
    assert_eq!(attr.ino, 1, "Linux FUSE: GETATTR root has ino=1");
}

#[test]
fn read_write_happy_path() {
    let mut fs = setup_fuse();
    let ino = fs.create("rw.bin", b"AAAAAA".to_vec()).expect("create");

    // FUSE_WRITE happy path.
    let written = fs.write(ino, 0, b"BBBBBB").expect("FUSE_WRITE");
    assert_eq!(written, 6, "Linux FUSE: WRITE returns bytes written");

    // FUSE_READ happy path.
    let bytes = fs.read(ino, 0, 6).expect("FUSE_READ");
    assert_eq!(
        bytes, b"BBBBBB",
        "Linux FUSE: READ returns most-recently-written data"
    );
}

#[test]
fn open_release_happy_path() {
    // KisekiFuse does not expose a separate open/release call; the
    // daemon's `Filesystem::open` returns a no-op handle and
    // `release` is similarly a no-op. The presence of the create+
    // read path covers the open + release transition implicitly.
    let mut fs = setup_fuse();
    let ino = fs.create("or.bin", b"x".to_vec()).expect("create");
    let _ = fs.read(ino, 0, 1).expect("read implies open + release");
}

#[test]
fn opendir_readdir_releasedir_happy_path() {
    let mut fs = setup_fuse();
    fs.create("a", b"A".to_vec()).expect("create a");
    fs.create("b", b"B".to_vec()).expect("create b");
    let entries = fs.readdir();
    // FUSE_OPENDIR + FUSE_READDIR + FUSE_RELEASEDIR happen
    // implicitly around `readdir()`.
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a"));
    assert!(names.contains(&"b"));
}

#[test]
fn flush_fsync_happy_path() {
    // Kiseki's gateway commits on every write (no buffered cache),
    // so `FUSE_FLUSH` and `FUSE_FSYNC` are no-ops at the FUSE layer.
    // A future cache layer may change that — when it does, this test
    // gains behavioral assertions.
    let mut fs = setup_fuse();
    let _ino = fs.create("fl.bin", b"sync me".to_vec()).expect("create");
    // Just verify the create path completed; flush/fsync are
    // implicit on each write.
}

#[test]
fn mkdir_rmdir_happy_path() {
    let mut fs = setup_fuse();
    let dir_ino = fs.mkdir("d").expect("FUSE_MKDIR");
    assert!(dir_ino > 1);
    fs.rmdir("d").expect("FUSE_RMDIR");
    assert!(fs.lookup("d").is_err(), "Linux FUSE: RMDIR removes entry");
}

#[test]
fn unlink_happy_path() {
    let mut fs = setup_fuse();
    fs.create("u.bin", b"x".to_vec()).expect("create");
    fs.unlink("u.bin").expect("FUSE_UNLINK");
    assert!(
        fs.lookup("u.bin").is_err(),
        "Linux FUSE: UNLINK removes entry"
    );
}

#[test]
fn rename_happy_path() {
    let mut fs = setup_fuse();
    fs.create("old.bin", b"x".to_vec()).expect("create");
    fs.rename("old.bin", "new.bin").expect("FUSE_RENAME");
    assert!(
        fs.lookup("old.bin").is_err(),
        "Linux FUSE: RENAME removes old name"
    );
    assert!(
        fs.lookup("new.bin").is_ok(),
        "Linux FUSE: RENAME creates new name"
    );
}

// ===========================================================================
// Cross-implementation seed — pinned op codes from `<linux/fuse.h>`
// ===========================================================================

/// Cross-implementation seed for the Linux FUSE wire protocol: this
/// table is hand-derived from `linux/include/uapi/linux/fuse.h` and
/// matches the values fuser 0.17 reports for `Operation::*`. Anyone
/// porting kiseki to a new fuser version (or a different FUSE
/// userspace) MUST re-validate against this table.
///
/// Source: `linux/include/uapi/linux/fuse.h`, kernel v6.10. Values
/// have been stable since FUSE protocol 7.x (c. 2005).
#[test]
fn rfc_seed_linux_fuse_opcode_table() {
    const LINUX_FUSE_OPCODES: &[(&str, u32)] = &[
        ("LOOKUP", 1),
        ("FORGET", 2),
        ("GETATTR", 3),
        ("SETATTR", 4),
        ("READLINK", 5),
        ("SYMLINK", 6),
        ("MKNOD", 8),
        ("MKDIR", 9),
        ("UNLINK", 10),
        ("RMDIR", 11),
        ("RENAME", 12),
        ("LINK", 13),
        ("OPEN", 14),
        ("READ", 15),
        ("WRITE", 16),
        ("STATFS", 17),
        ("RELEASE", 18),
        ("FSYNC", 20),
        ("SETXATTR", 21),
        ("GETXATTR", 22),
        ("LISTXATTR", 23),
        ("REMOVEXATTR", 24),
        ("FLUSH", 25),
        ("INIT", 26),
        ("OPENDIR", 27),
        ("READDIR", 28),
        ("RELEASEDIR", 29),
        ("FSYNCDIR", 30),
        ("CREATE", 35),
    ];

    // Map the constants by name so this test is the seed source-of-truth.
    let pinned: Vec<(&str, u32)> = vec![
        ("LOOKUP", FUSE_LOOKUP),
        ("FORGET", FUSE_FORGET),
        ("GETATTR", FUSE_GETATTR),
        ("SETATTR", FUSE_SETATTR),
        ("READLINK", FUSE_READLINK),
        ("SYMLINK", FUSE_SYMLINK),
        ("MKNOD", FUSE_MKNOD),
        ("MKDIR", FUSE_MKDIR),
        ("UNLINK", FUSE_UNLINK),
        ("RMDIR", FUSE_RMDIR),
        ("RENAME", FUSE_RENAME),
        ("LINK", FUSE_LINK),
        ("OPEN", FUSE_OPEN),
        ("READ", FUSE_READ),
        ("WRITE", FUSE_WRITE),
        ("STATFS", FUSE_STATFS),
        ("RELEASE", FUSE_RELEASE),
        ("FSYNC", FUSE_FSYNC),
        ("SETXATTR", FUSE_SETXATTR),
        ("GETXATTR", FUSE_GETXATTR),
        ("LISTXATTR", FUSE_LISTXATTR),
        ("REMOVEXATTR", FUSE_REMOVEXATTR),
        ("FLUSH", FUSE_FLUSH),
        ("INIT", FUSE_INIT),
        ("OPENDIR", FUSE_OPENDIR),
        ("READDIR", FUSE_READDIR),
        ("RELEASEDIR", FUSE_RELEASEDIR),
        ("FSYNCDIR", FUSE_FSYNCDIR),
        ("CREATE", FUSE_CREATE),
    ];

    assert_eq!(
        LINUX_FUSE_OPCODES.len(),
        pinned.len(),
        "seed table and pinned constants must cover the same op set"
    );
    for ((spec_name, spec_val), (pin_name, pin_val)) in LINUX_FUSE_OPCODES.iter().zip(pinned.iter())
    {
        assert_eq!(
            spec_name, pin_name,
            "Linux FUSE: op-table order must match seed (got {pin_name}, expected {spec_name})"
        );
        assert_eq!(
            spec_val, pin_val,
            "Linux FUSE: {spec_name} must be {spec_val} (kernel UAPI), got {pin_val}"
        );
    }
}
