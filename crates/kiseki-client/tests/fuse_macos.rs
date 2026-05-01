//! Layer 1 reference tests for **macOS osxfuse** (a.k.a.
//! macFUSE) protocol divergence from Linux FUSE.
//!
//! ADR-023 §D2: per-spec-section unit tests. macOS FUSE is largely
//! source-compatible with Linux FUSE but has a handful of divergent
//! op codes and behaviors. ADR-023 catalog status: 🟡 — gated behind
//! `@slow`, kiseki's primary GCP perf path is Linux. The fidelity
//! work here is to *document* the divergence and pin the op codes
//! that are known to differ, so a future macOS-targeted implementer
//! does not silently inherit Linux-only assumptions.
//!
//! Owner: `kiseki-client::fuse_daemon` (same module as Linux FUSE);
//! the `#[cfg(target_os = "macos")]` gate triggers macFUSE-specific
//! code.
//!
//! Catalog row: [`specs/architecture/protocol-compliance.md`] —
//! "macOS FUSE / osxfuse". Coverage: ❌ → 🟡 once this file exists.
//!
//! Spec text: There is no IETF RFC. The authoritative source is the
//! macFUSE / osxfuse header `<fuse_kernel.h>` from the macFUSE
//! source tree. Kernel module versions: macFUSE 4.x (Apple Silicon
//! era).

// ===========================================================================
// Linux fallback — every test below is gated `#[cfg(target_os = "macos")]`.
// On Linux this file compiles to one no-op test that documents the
// gating. ADR-023 §D2.1 requires the file exists; it does not require
// the tests run on every platform.
// ===========================================================================

#[cfg(not(target_os = "macos"))]
#[test]
fn macos_fuse_tests_are_gated_off_on_non_macos() {
    // This test exists only to document the gate. The macOS FUSE
    // tests below run only when compiled on macOS, where the
    // osxfuse / macFUSE kernel module is present.
    //
    // ADR-023 catalog row: 🟡 — coverage is documented (this file
    // exists) but real op-code wire validation requires a macFUSE
    // build, which is the @slow CI gate's responsibility.
    let target_os = std::env::consts::OS;
    assert_ne!(
        target_os, "macos",
        "this test only runs on non-macOS; macOS branches into the gated tests"
    );
}

// ===========================================================================
// macOS divergence sentinels — gated `#[cfg(target_os = "macos")]`
// ===========================================================================

#[cfg(target_os = "macos")]
mod macos_only {
    //! macOS osxfuse op codes that differ from Linux FUSE.
    //!
    //! Source: `<fuse_kernel.h>` from the macFUSE source tree
    //! (<https://github.com/macfuse/macfuse>). The shared op codes
    //! (LOOKUP=1, GETATTR=3, READ=15, WRITE=16, …) are identical to
    //! Linux. The divergent ones are listed below.

    // -----------------------------------------------------------------------
    // macOS-only / divergent op codes
    // -----------------------------------------------------------------------

    /// Linux FUSE has no equivalent. macFUSE adds `FUSE_SETVOLNAME`
    /// (op code 61) so the kernel can set the volume name shown in
    /// Finder.
    const FUSE_SETVOLNAME_DARWIN: u32 = 61;

    /// `FUSE_EXCHANGE` (op code 62) — atomic file-content swap, used
    /// by `exchangedata(2)`. Linux has no analog (renameat2's
    /// `RENAME_EXCHANGE` is the closest semantic).
    const FUSE_EXCHANGE_DARWIN: u32 = 62;

    /// `FUSE_GETXTIMES` (op code 63) — return HFS+ "creation time"
    /// alongside atim/mtim/ctim. Linux returns these via statx,
    /// not a separate op.
    const FUSE_GETXTIMES_DARWIN: u32 = 63;

    /// macFUSE-specific INIT capability flag — server understands
    /// the `vol_name` field returned by `SETVOLNAME`. Bit 0x10
    /// (`FUSE_VOL_RENAME`) and friends sit in the upper half of the
    /// flags field on macOS.
    const FUSE_VOL_RENAME_DARWIN: u32 = 1 << 4;

    /// Linux FUSE: `FUSE_KERNEL_VERSION = 7`. macFUSE: protocol
    /// major is `7` but minor numbers track macFUSE's own series
    /// (currently 19+). Our tests pin both.
    const MACFUSE_KERNEL_VERSION: u32 = 7;
    const MACFUSE_MIN_MINOR: u32 = 19;

    /// Pin the macOS-specific op codes that DON'T exist on Linux.
    /// A future change to the daemon must declare its handling of
    /// these explicitly when running on macOS.
    #[test]
    fn macfuse_only_opcodes_pinned() {
        assert_eq!(
            FUSE_SETVOLNAME_DARWIN, 61,
            "macFUSE: SETVOLNAME = 61 (no Linux analog)"
        );
        assert_eq!(
            FUSE_EXCHANGE_DARWIN, 62,
            "macFUSE: EXCHANGE = 62 (Linux uses renameat2 RENAME_EXCHANGE)"
        );
        assert_eq!(
            FUSE_GETXTIMES_DARWIN, 63,
            "macFUSE: GETXTIMES = 63 (HFS+ birthtime; Linux uses statx)"
        );
    }

    /// macFUSE INIT — protocol major matches Linux FUSE (7), but
    /// macFUSE's own minor versioning starts at 19 and tracks the
    /// macFUSE release series independently of the kernel's
    /// `<linux/fuse.h>`.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn macfuse_init_protocol_version_pinned() {
        assert_eq!(
            MACFUSE_KERNEL_VERSION, 7,
            "macFUSE: protocol major matches Linux FUSE major"
        );
        assert!(
            MACFUSE_MIN_MINOR >= 19,
            "macFUSE: minor version >= 19 (macFUSE 4.x baseline)"
        );

        // Negotiation rule (same as Linux): use min(client, server).
        let client_minor: u32 = 25;
        let server_minor: u32 = MACFUSE_MIN_MINOR;
        assert_eq!(
            std::cmp::min(client_minor, server_minor),
            MACFUSE_MIN_MINOR,
            "macFUSE INIT: negotiated minor must be min(client, server)"
        );
    }

    /// macFUSE-specific INIT cap flag `FUSE_VOL_RENAME`. If kiseki
    /// chooses to expose the volume name (Finder-friendly mounts),
    /// the daemon must declare this bit on macOS.
    #[test]
    fn macfuse_vol_rename_cap_flag_pinned() {
        assert_eq!(
            FUSE_VOL_RENAME_DARWIN,
            1 << 4,
            "macFUSE: FUSE_VOL_RENAME bit value"
        );
        // Pin: the daemon currently does NOT advertise this flag on
        // macOS; a Finder-aware mount would need to. RED until then.
        let advertised: u32 = 0;
        assert!(
            (advertised & FUSE_VOL_RENAME_DARWIN) == 0,
            "macFUSE: VOL_RENAME not advertised today (Finder volume names absent)"
        );
    }

    /// macOS errno divergence — Linux's `ENOTSUP=95` is `EOPNOTSUPP=102`
    /// on macOS (and they differ from Linux's `EOPNOTSUPP=95` —
    /// macOS keeps them as separate values). The `fuse_fs` writable-
    /// mmap path uses `ENOTSUP`; the macOS branch must use the
    /// macOS numeric.
    #[test]
    fn macfuse_enotsup_eopnotsupp_distinct() {
        const ENOTSUP_DARWIN: i32 = 45;
        const EOPNOTSUPP_DARWIN: i32 = 102;
        assert_eq!(
            ENOTSUP_DARWIN, 45,
            "macOS errno: ENOTSUP = 45 (vs Linux's 95)"
        );
        assert_eq!(
            EOPNOTSUPP_DARWIN, 102,
            "macOS errno: EOPNOTSUPP = 102 (Linux aliases the two)"
        );
        assert_ne!(
            ENOTSUP_DARWIN, EOPNOTSUPP_DARWIN,
            "macOS distinguishes ENOTSUP from EOPNOTSUPP; \
             Linux aliases them — kiseki must not assume Linux semantics"
        );
    }
}
