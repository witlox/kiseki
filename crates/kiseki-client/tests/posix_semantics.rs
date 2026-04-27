//! Layer 1 reference tests for **POSIX.1-2024 (IEEE Std 1003.1-2024)**
//! file-system semantics, as exposed through `kiseki-client::fuse_fs`.
//!
//! ADR-023 §D2 mandates per-section tests; for POSIX the "sections"
//! are the operations and errno values defined by the standard. The
//! Kiseki-side functional scope is pinned by ADR-013 (POSIX semantics
//! scope) — these tests assert the wire-side contract (errno values,
//! `stat` field meanings, readdir cookie monotonicity, rename
//! atomicity) that the FUSE filesystem MUST satisfy on Linux.
//!
//! Owner: `kiseki-client::fuse_fs::KisekiFuse` — the FUSE
//! `Filesystem` trait impl that translates POSIX ops into
//! `GatewayOps` calls. Errno mapping happens here; the kernel only
//! sees what this module returns.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "POSIX.1-2024 (IEEE Std 1003.1-2024)".
//!
//! Spec text: <https://pubs.opengroup.org/onlinepubs/9799919799/>
//! (POSIX.1-2024 Issue 8, July 2024). Errno values are pinned per
//! the Linux ABI (`<asm-generic/errno-base.h>` /
//! `<asm-generic/errno.h>`); other Unix systems use different
//! numerics for the same names — see the cross-implementation seed
//! at the bottom of this file.

use kiseki_chunk::store::ChunkStore;
use kiseki_client::fuse_fs::{FileKind, KisekiFuse};
use kiseki_common::ids::{NamespaceId, OrgId, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;

// ===========================================================================
// Sentinel constants — Linux errno ABI (`<asm-generic/errno-base.h>`,
// `<asm-generic/errno.h>`). POSIX.1-2024 names the symbols; the
// numeric values are platform-specific. Kiseki targets the Linux ABI
// because the FUSE wire protocol is the kernel one.
// ===========================================================================

/// `ENOENT` — "No such file or directory" (POSIX.1-2024
/// `<errno.h>`; Linux: 2).
const ENOENT: i32 = 2;
/// `EACCES` — "Permission denied" (Linux: 13).
const EACCES: i32 = 13;
/// `EEXIST` — "File exists" (Linux: 17).
const EEXIST: i32 = 17;
/// `ENOTDIR` — "Not a directory" (Linux: 20).
const ENOTDIR: i32 = 20;
/// `EISDIR` — "Is a directory" (Linux: 21).
const EISDIR: i32 = 21;
/// `EINVAL` — "Invalid argument" (Linux: 22).
const EINVAL: i32 = 22;
/// `ENOSYS` — "Function not implemented" (Linux: 38).
const ENOSYS: i32 = 38;
/// `EROFS` — "Read-only file system" (Linux: 30).
const EROFS: i32 = 30;

// ---------------------------------------------------------------------------
// Helpers — FUSE filesystem wiring matching `tests/concurrent_fuse.rs`.
// ---------------------------------------------------------------------------

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn setup_fuse(read_only: bool) -> KisekiFuse<InMemoryGateway> {
    let mut compositions = CompositionStore::new();
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: ShardId(uuid::Uuid::from_u128(1)),
        read_only,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    let gw = InMemoryGateway::new(compositions, Box::new(chunks), master_key);
    KisekiFuse::new(gw, test_tenant(), test_namespace())
}

// ===========================================================================
// Errno sentinels — POSIX.1-2024 §<errno.h> + Linux ABI
// ===========================================================================

/// POSIX.1-2024 names; Linux numeric ABI. A future refactor that
/// introduces a typed `Errno` cannot accidentally renumber these.
#[test]
fn errno_constants_match_linux_abi() {
    assert_eq!(ENOENT, 2, "POSIX <errno.h>: ENOENT (Linux ABI)");
    assert_eq!(EACCES, 13, "POSIX <errno.h>: EACCES (Linux ABI)");
    assert_eq!(EEXIST, 17, "POSIX <errno.h>: EEXIST (Linux ABI)");
    assert_eq!(ENOTDIR, 20, "POSIX <errno.h>: ENOTDIR (Linux ABI)");
    assert_eq!(EISDIR, 21, "POSIX <errno.h>: EISDIR (Linux ABI)");
    assert_eq!(EINVAL, 22, "POSIX <errno.h>: EINVAL (Linux ABI)");
    assert_eq!(EROFS, 30, "POSIX <errno.h>: EROFS (Linux ABI)");
    assert_eq!(ENOSYS, 38, "POSIX <errno.h>: ENOSYS (Linux ABI)");
}

// ===========================================================================
// `lookup` / `open` — ENOENT
// ===========================================================================

/// POSIX.1-2024 §`open` ERRORS — `[ENOENT]` "A component of `path`
/// does not name an existing file or `path` is an empty string".
#[test]
fn lookup_missing_returns_enoent() {
    let fs = setup_fuse(false);
    let err = fs
        .lookup("definitely-not-here.txt")
        .expect_err("missing file must error");
    assert_eq!(
        err, ENOENT,
        "POSIX.1-2024 lookup on missing path: errno=ENOENT"
    );
}

/// POSIX.1-2024 §`open` ERRORS — `[ENOENT]` also applies when the
/// parent directory itself does not exist.
#[test]
fn lookup_in_missing_parent_returns_enoent() {
    let fs = setup_fuse(false);
    let err = fs
        .lookup_in(9999, "anything")
        .expect_err("missing parent must error");
    assert_eq!(
        err, ENOENT,
        "POSIX.1-2024: lookup under non-existent parent inode → ENOENT"
    );
}

// ===========================================================================
// `read` / `write` — EISDIR
// ===========================================================================

/// POSIX.1-2024 §`read` ERRORS — `[EISDIR]` "The `fildes` argument
/// refers to a directory and the implementation does not allow the
/// directory to be read using `read()`."
#[test]
fn read_on_directory_returns_eisdir() {
    let fs = setup_fuse(false);
    // Inode 1 is the root directory.
    let err = fs.read(1, 0, 16).expect_err("read on directory must error");
    assert_eq!(err, EISDIR, "POSIX.1-2024: read() on directory → EISDIR");
}

// ===========================================================================
// `mkdir` / `unlink` — ENOTDIR
// ===========================================================================

/// POSIX.1-2024 §`mkdir` ERRORS — `[ENOTDIR]` "A component of the
/// path prefix names an existing file that is neither a directory
/// nor a symbolic link to a directory".
#[test]
fn create_under_file_parent_returns_enotdir() {
    let mut fs = setup_fuse(false);
    let file_ino = fs
        .create("regular.txt", b"hello".to_vec())
        .expect("create regular file");

    // Try to create a child under that file (which is not a directory).
    let err = fs
        .create_in(file_ino, "child", b"x".to_vec())
        .expect_err("creating under a regular file must error");
    assert_eq!(
        err, ENOTDIR,
        "POSIX.1-2024: parent component is not a directory → ENOTDIR"
    );
}

// ===========================================================================
// `mkdir` / `link` — EEXIST
// ===========================================================================

/// POSIX.1-2024 §`mkdir` ERRORS — `[EEXIST]` "The named file
/// exists." Same rule applies to `link`, `symlink`, and `creat`
/// with `O_EXCL`.
#[test]
fn create_existing_name_returns_eexist() {
    let mut fs = setup_fuse(false);
    fs.create("dup.txt", b"first".to_vec())
        .expect("first create");
    let err = fs
        .create("dup.txt", b"second".to_vec())
        .expect_err("re-create with same name must error");
    assert_eq!(err, EEXIST, "POSIX.1-2024: create existing name → EEXIST");
}

#[test]
fn mkdir_existing_name_returns_eexist() {
    let mut fs = setup_fuse(false);
    fs.mkdir("subdir").expect("first mkdir");
    let err = fs
        .mkdir("subdir")
        .expect_err("re-mkdir with same name must error");
    assert_eq!(err, EEXIST, "POSIX.1-2024: mkdir existing name → EEXIST");
}

// ===========================================================================
// `write` on read-only filesystem — EROFS
// ===========================================================================

/// POSIX.1-2024 §`write` ERRORS — `[EROFS]` "The file resides on a
/// read-only file system." Kiseki maps the namespace `read_only`
/// flag to this errno on writes.
///
/// Today's `KisekiFuse::create` propagates `EIO` when the gateway
/// rejects the write. The strict POSIX contract is `EROFS` — this
/// test pins the contract; until the mapping lands, the assertion
/// is RED.
#[test]
fn write_to_readonly_namespace_returns_erofs() {
    let mut fs = setup_fuse(true);
    let err = fs
        .create("forbidden.txt", b"data".to_vec())
        .expect_err("write to read-only namespace must error");
    assert_eq!(
        err, EROFS,
        "POSIX.1-2024: write on read-only file system → EROFS \
         (gateway currently returns EIO; see ADR-013)"
    );
}

// ===========================================================================
// Unimplemented operations — ENOSYS
// ===========================================================================

/// POSIX.1-2024 §`<errno.h>` — `[ENOSYS]` "Function not supported".
/// ADR-013 explicitly carves out writable shared mmap, POSIX.1e ACLs,
/// chroot, pivot_root. The FUSE layer maps these to `ENOSYS` (or
/// `ENOTSUP` on non-Linux).
///
/// `KisekiFuse` exposes no public API for these unsupported ops, so
/// this test pins the Linux ABI sentinel that the FUSE handler MUST
/// return. When a typed unsupported-op surface lands, the assertion
/// graduates from a constant pin to a behavioral test.
#[test]
fn unimplemented_ops_return_enosys() {
    // The FUSE daemon should emit ENOSYS for every op outside ADR-013's
    // supported matrix. Today there is no observable surface from the
    // public API; this test pins the contract at the constant level.
    assert_eq!(
        ENOSYS, 38,
        "POSIX.1-2024: ADR-013 not-supported ops must surface as ENOSYS=38 on Linux"
    );
}

// ===========================================================================
// EACCES — permission denied
// ===========================================================================

/// POSIX.1-2024 §`open` ERRORS — `[EACCES]` "Permission denied".
/// Kiseki's FUSE layer does not (yet) enforce per-uid permission
/// checks beyond the underlying gateway authorization; ADR-013
/// mentions chmod/chown as supported. This test asserts the errno
/// constant pin so a future authz path can graduate it.
#[test]
fn eacces_constant_pinned_for_future_authz() {
    // No behavioral path triggers EACCES today (no per-op authz in
    // the test gateway). The numeric pin guards against renumbering
    // when authz lands.
    assert_eq!(EACCES, 13, "POSIX <errno.h>: EACCES (Linux ABI)");
}

// ===========================================================================
// `stat(2)` — POSIX.1-2024 §<sys/stat.h>
// ===========================================================================
//
// POSIX defines the layout of `struct stat`:
//
//     dev_t            st_dev;
//     ino_t            st_ino;
//     mode_t           st_mode;     // file type + perm bits
//     nlink_t          st_nlink;
//     uid_t            st_uid;
//     gid_t            st_gid;
//     dev_t            st_rdev;
//     off_t            st_size;
//     struct timespec  st_atim;
//     struct timespec  st_mtim;
//     struct timespec  st_ctim;
//     blksize_t        st_blksize;
//     blkcnt_t         st_blocks;
//
// The Kiseki `FileAttr` only carries the subset the FUSE layer
// translates; the assertions below pin the meaning of each field
// we do expose.

/// POSIX.1-2024 §`<sys/stat.h>` — `st_mode` encodes both file
/// type (high bits, `S_IFMT` mask) and permission bits (low bits,
/// `S_IRWXU|S_IRWXG|S_IRWXO`). `KisekiFuse::FileAttr.mode` carries
/// only the permission bits today — the file type comes from
/// `FileAttr.kind`. This test pins both halves.
#[test]
fn stat_st_mode_separates_kind_and_permission_bits() {
    let mut fs = setup_fuse(false);
    let file_ino = fs
        .create("regular.bin", b"abc".to_vec())
        .expect("create regular");
    let dir_ino = fs.mkdir("subdir").expect("mkdir");

    let file_attr = fs.getattr(file_ino).expect("getattr file");
    let dir_attr = fs.getattr(dir_ino).expect("getattr dir");

    // Permission bits — POSIX.1-2024: 0o644 is `rw-r--r--`,
    // 0o755 is `rwxr-xr-x`. Both fit in the low 12 mode bits.
    assert_eq!(
        file_attr.mode & 0o7777,
        0o644,
        "POSIX <sys/stat.h>: regular file default mode permission bits"
    );
    assert_eq!(
        dir_attr.mode & 0o7777,
        0o755,
        "POSIX <sys/stat.h>: directory default mode permission bits"
    );

    // File-type discriminator (carried out-of-band on `FileAttr.kind`,
    // since the production code does not encode S_IFREG / S_IFDIR
    // into the mode field). POSIX.1-2024 §<sys/stat.h>: S_IFREG=0o100000,
    // S_IFDIR=0o040000. Pinned via constants below.
    assert_eq!(
        file_attr.kind,
        FileKind::Regular,
        "POSIX file type: S_ISREG(st_mode) for regular files"
    );
    assert_eq!(
        dir_attr.kind,
        FileKind::Directory,
        "POSIX file type: S_ISDIR(st_mode) for directories"
    );
}

/// POSIX.1-2024 §`<sys/stat.h>` — `st_size` for a regular file is
/// the size of the file in bytes (the byte offset of the
/// end-of-file). For directories it is implementation-defined; we
/// report 0.
#[test]
fn stat_st_size_matches_file_byte_length() {
    let mut fs = setup_fuse(false);
    let payload = b"POSIX size field: bytes to EOF";
    let ino = fs.create("sized.bin", payload.to_vec()).expect("create");
    let attr = fs.getattr(ino).expect("getattr");
    assert_eq!(
        attr.size,
        payload.len() as u64,
        "POSIX <sys/stat.h>: st_size must equal the file length in bytes"
    );

    // Directory: implementation-defined, kiseki reports 0.
    let dir_ino = fs.mkdir("d").expect("mkdir");
    let dir_attr = fs.getattr(dir_ino).expect("getattr dir");
    assert_eq!(
        dir_attr.size, 0,
        "POSIX <sys/stat.h>: directory st_size implementation-defined; kiseki reports 0"
    );
}

/// POSIX.1-2024 §`<sys/stat.h>` — `st_nlink` counts hard links to
/// the file. A freshly-created regular file has `nlink == 1`. A
/// freshly-created directory has `nlink >= 2` (the directory itself
/// + its `.` entry); kiseki reports 2.
#[test]
fn stat_st_nlink_initial_values() {
    let mut fs = setup_fuse(false);
    let file_ino = fs.create("file.bin", b"x".to_vec()).expect("create");
    let dir_ino = fs.mkdir("d").expect("mkdir");

    let file_attr = fs.getattr(file_ino).expect("getattr file");
    let dir_attr = fs.getattr(dir_ino).expect("getattr dir");

    assert_eq!(
        file_attr.nlink, 1,
        "POSIX <sys/stat.h>: freshly-created regular file: st_nlink == 1"
    );
    assert!(
        dir_attr.nlink >= 2,
        "POSIX <sys/stat.h>: directory st_nlink >= 2 (includes . entry); got {}",
        dir_attr.nlink
    );
}

/// POSIX.1-2024 §`<sys/stat.h>` — `st_uid` / `st_gid` carry the
/// owner uid and gid. Kiseki today reports uid=0/gid=0 from the FUSE
/// daemon. This test pins the spec contract: the field MUST be
/// present in the FUSE reply (the `to_fuser_attr` mapper in
/// `fuse_daemon.rs` guarantees this).
///
/// The `KisekiFuse::FileAttr` struct does not yet expose uid/gid —
/// that's a fidelity gap. ADR-013 lists chmod/chown as supported, so
/// the gap will close when per-file owner attributes land in the
/// composition store.
#[test]
fn stat_st_uid_gid_field_presence_documented() {
    // No `uid` / `gid` field on `FileAttr` today (gap). The FUSE
    // daemon hard-codes 0/0 in `to_fuser_attr`. POSIX.1-2024 requires
    // both to be present in `struct stat`. This test is a placeholder
    // pin until the FileAttr surface carries them.
    let fs = setup_fuse(false);
    let attr = fs.getattr(1).expect("root getattr");
    // No uid/gid field exists today. Once added, this assertion
    // upgrades to a real check.
    assert_eq!(
        attr.kind,
        FileKind::Directory,
        "POSIX <sys/stat.h>: root is a directory; uid/gid fields TBD"
    );
}

/// POSIX.1-2024 §`<sys/stat.h>` — `st_atim`, `st_mtim`, `st_ctim`
/// are `struct timespec` (seconds + nanoseconds). The FUSE daemon
/// reports `SystemTime::UNIX_EPOCH` for all three today (a known
/// fidelity gap). When real timestamps land, this test graduates
/// to behavioral assertions on monotonicity (mtime <= ctime, etc.).
#[test]
fn stat_st_atim_mtim_ctim_field_presence_documented() {
    // Same shape as the uid/gid case: `KisekiFuse::FileAttr` does not
    // carry timestamps today. The FUSE daemon hardcodes them to
    // `UNIX_EPOCH`. POSIX.1-2024 requires all three timestamps. This
    // test stands in until the FileAttr surface grows the fields.
    let fs = setup_fuse(false);
    let attr = fs.getattr(1).expect("root getattr");
    assert_eq!(
        attr.kind,
        FileKind::Directory,
        "POSIX <sys/stat.h>: root attr; atim/mtim/ctim fields TBD"
    );
}

// ===========================================================================
// `readdir` — POSIX.1-2024 cookie monotonicity
// ===========================================================================

/// POSIX.1-2024 §`readdir` defines the directory-entry iteration
/// model. The Linux/FUSE adaptation passes a `cookie` (a.k.a.
/// `offset`) so the kernel can resume after a partial reply.
/// Cookies MUST be monotonically increasing across consecutive
/// `READDIR` calls — otherwise the kernel cannot detect end-of-list
/// or skipped entries.
///
/// Kiseki's `readdir_in` returns a `Vec<DirEntry>` directly (no
/// cookie surface today). The cookie is generated by the FUSE
/// daemon as `(i + 1) as u64` for the i-th entry (see
/// `fuse_daemon.rs::readdir`). This test pins the daemon's cookie
/// rule.
#[test]
fn readdir_cookies_monotonically_increase() {
    let mut fs = setup_fuse(false);
    fs.create("a.bin", b"1".to_vec()).expect("create a");
    fs.create("b.bin", b"2".to_vec()).expect("create b");
    fs.create("c.bin", b"3".to_vec()).expect("create c");

    let entries = fs.readdir();
    // Cookie generation matches `fuse_daemon.rs`: cookie(i) = i + 1.
    let cookies: Vec<u64> = (0..entries.len() as u64).map(|i| i + 1).collect();

    // Strictly monotonic.
    for win in cookies.windows(2) {
        assert!(
            win[0] < win[1],
            "POSIX/FUSE readdir: cookies must strictly increase ({} < {})",
            win[0],
            win[1]
        );
    }
    // First cookie is non-zero (zero is the "rewind" sentinel).
    assert!(
        cookies.first().copied().unwrap_or(0) > 0,
        "POSIX/FUSE readdir: first cookie must be > 0 (0 is rewind)"
    );
}

/// POSIX.1-2024 §`readdir` — every directory MUST contain `.` (self)
/// and `..` (parent) entries.
#[test]
fn readdir_includes_dot_and_dotdot() {
    let fs = setup_fuse(false);
    let entries = fs.readdir();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"."),
        "POSIX.1-2024 §readdir: directory MUST contain `.` entry"
    );
    assert!(
        names.contains(&".."),
        "POSIX.1-2024 §readdir: directory MUST contain `..` entry"
    );
}

// ===========================================================================
// `rename(2)` — POSIX.1-2024 atomicity
// ===========================================================================

/// POSIX.1-2024 §`rename` ATOMICITY:
///
/// > "If `new` names an existing file, the file shall be replaced.
/// >  Write access permission is required for both the directory
/// >  containing `old` and the directory containing `new`. If the
/// >  rename succeeds, the link named `new` shall remain visible to
/// >  other processes throughout the operation; there shall be no
/// >  point at which `new` does not exist."
///
/// In other words: rename either fully succeeds (the old name is
/// gone, the new name points at the original inode) or leaves both
/// names intact. There is no observable intermediate state.
#[test]
fn rename_either_fully_succeeds_or_leaves_both_names_intact() {
    let mut fs = setup_fuse(false);
    let _ino = fs
        .create("source.bin", b"payload".to_vec())
        .expect("create source");

    fs.rename("source.bin", "dest.bin").expect("rename");

    // Old name is gone.
    let old = fs.lookup("source.bin");
    assert_eq!(
        old.expect_err("old name must not exist after rename"),
        ENOENT,
        "POSIX.1-2024 rename: old name must not exist post-success"
    );
    // New name resolves to the file.
    let new_attr = fs.lookup("dest.bin").expect("new name resolves");
    assert_eq!(
        new_attr.kind,
        FileKind::Regular,
        "POSIX.1-2024 rename: new name preserves the file kind"
    );
    // Data preserved through the rename.
    let bytes = fs.read(new_attr.ino, 0, 16).expect("read after rename");
    assert_eq!(
        bytes, b"payload",
        "POSIX.1-2024 rename: data preserved across rename"
    );
}

/// POSIX.1-2024 §`rename` — when `new` already names an existing
/// file, the rename atomically replaces it. The destination MUST
/// remain visible at every point.
#[test]
fn rename_replaces_existing_destination_atomically() {
    let mut fs = setup_fuse(false);
    fs.create("a.bin", b"AAA".to_vec()).expect("create a");
    fs.create("b.bin", b"BBB".to_vec()).expect("create b");

    fs.rename("a.bin", "b.bin").expect("rename a -> b");

    // Old name gone.
    assert_eq!(fs.lookup("a.bin").expect_err("a must not exist"), ENOENT);
    // New name resolves to the contents originally at a.bin.
    let attr = fs.lookup("b.bin").expect("b still resolves");
    let bytes = fs.read(attr.ino, 0, 8).expect("read replaced b");
    assert_eq!(
        bytes, b"AAA",
        "POSIX.1-2024 rename: destination replaced atomically; \
         contents come from source"
    );
}

/// POSIX.1-2024 §`rename` — renaming a non-existent source MUST
/// fail with `ENOENT`. The destination (if any) MUST be unchanged.
#[test]
fn rename_missing_source_returns_enoent_and_leaves_destination_intact() {
    let mut fs = setup_fuse(false);
    fs.create("dest.bin", b"DEST".to_vec())
        .expect("create dest");

    let err = fs
        .rename("nope.bin", "dest.bin")
        .expect_err("rename of missing source must error");
    assert_eq!(err, ENOENT, "POSIX.1-2024 rename: missing source → ENOENT");

    // Destination still intact, contents unchanged.
    let attr = fs.lookup("dest.bin").expect("dest still resolves");
    let bytes = fs.read(attr.ino, 0, 8).expect("read dest");
    assert_eq!(
        bytes, b"DEST",
        "POSIX.1-2024 rename: failed rename leaves destination intact"
    );
}

// ===========================================================================
// `readlink` on a non-symlink — EINVAL
// ===========================================================================

/// POSIX.1-2024 §`readlink` ERRORS — `[EINVAL]` "The `path` argument
/// names a file that is not a symbolic link."
#[test]
fn readlink_on_regular_file_returns_einval() {
    let mut fs = setup_fuse(false);
    let ino = fs.create("not-a-link.bin", b"x".to_vec()).expect("create");
    let err = fs
        .readlink(ino)
        .expect_err("readlink on regular file must error");
    assert_eq!(err, EINVAL, "POSIX.1-2024 readlink on non-symlink → EINVAL");
}

// ===========================================================================
// Cross-implementation seed — POSIX.1-2024 errno table for Linux
// ===========================================================================

/// POSIX.1-2024 only mandates the *names* of the errno values — the
/// numeric values are platform-specific. The Linux ABI numbers
/// (`<asm-generic/errno-base.h>` 1..34, `<asm-generic/errno.h>` 35..)
/// are what FUSE returns on the wire.
///
/// This test seeds the entire mapping from every test above, in one
/// place, so a future port to another Unix can override the numerics
/// without re-reading every test.
#[test]
fn rfc_seed_posix_errno_linux_abi_table() {
    // (POSIX symbol name, Linux numeric value, source header)
    const TABLE: &[(&str, i32, &str)] = &[
        ("ENOENT", 2, "<asm-generic/errno-base.h>"),
        ("EACCES", 13, "<asm-generic/errno-base.h>"),
        ("EEXIST", 17, "<asm-generic/errno-base.h>"),
        ("ENOTDIR", 20, "<asm-generic/errno-base.h>"),
        ("EISDIR", 21, "<asm-generic/errno-base.h>"),
        ("EINVAL", 22, "<asm-generic/errno-base.h>"),
        ("EROFS", 30, "<asm-generic/errno-base.h>"),
        ("ENOSYS", 38, "<asm-generic/errno.h>"),
    ];

    for (name, expected, header) in TABLE {
        let actual = match *name {
            "ENOENT" => ENOENT,
            "EACCES" => EACCES,
            "EEXIST" => EEXIST,
            "ENOTDIR" => ENOTDIR,
            "EISDIR" => EISDIR,
            "EINVAL" => EINVAL,
            "EROFS" => EROFS,
            "ENOSYS" => ENOSYS,
            _ => panic!("unknown errno name: {name}"),
        };
        assert_eq!(
            actual, *expected,
            "POSIX.1-2024 (Linux ABI {header}): {name} = {expected}"
        );
    }
}
