//! C-ABI link verification (ADV-PA-5).
//!
//! Compiles `c-abi-shim/abi_check.c` against the Kiseki C header
//! (`include/kiseki_client.h`) using the system C compiler. Three
//! things break this test:
//!
//! 1. **Enum drift** — `_Static_assert` calls in the shim verify
//!    every `KisekiStatus` discriminant. If the Rust `repr(C)` enum
//!    is renumbered, the C compile fails.
//! 2. **Struct rename / field removal** — `offsetof(KisekiCacheStats,
//!    field_x)` calls fail to compile if `field_x` is renamed or
//!    removed in the C header (which must be kept in sync with the
//!    Rust struct).
//! 3. **Function signature drift** — the shim takes function pointers
//!    of the documented types; a mismatched arity / param-type in
//!    the header surfaces as a compile error.
//!
//! What this test does NOT catch (out of scope for the
//! compile-only path):
//! - Renaming the Rust extern fn (no link step here). For full
//!   link-time symbol-presence verification, downstream wrappers
//!   (`crates/kiseki-client/src/python.rs` PyO3 bindings, the C++
//!   wrapper crate) link the cdylib in their own CI; renaming a
//!   symbol breaks their build.
//!
//! This test is the structural ABI check; the link-time check is
//! delegated to the wrapper crates.
#![allow(clippy::doc_markdown)]
#![cfg(feature = "ffi")]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn c_header_compiles_against_abi_shim() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let header_dir = manifest.join("include");
    let shim = manifest.join("c-abi-shim").join("abi_check.c");

    assert!(
        header_dir.join("kiseki_client.h").exists(),
        "kiseki_client.h not found at expected path"
    );
    assert!(shim.exists(), "abi_check.c not found at expected path");

    // Locate a C compiler. Prefer cc/gcc/clang in that order.
    let candidates = ["cc", "gcc", "clang"];
    let cc = candidates
        .iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok())
        .copied()
        .expect("no C compiler (cc/gcc/clang) found in PATH — Rust toolchain build environment is incomplete");

    // -fsyntax-only: parse + check static asserts but don't link.
    // This is exactly what we want — enum / struct / signature drift
    // surfaces here without needing a usable cdylib.
    let out = Command::new(cc)
        .arg("-fsyntax-only")
        .arg("-Wall")
        .arg("-Werror")
        .arg("-I")
        .arg(&header_dir)
        .arg(&shim)
        .output()
        .expect("invoke C compiler");

    assert!(
        out.status.success(),
        "C ABI shim failed to compile. stdout:\n{}\nstderr:\n{}\n\n\
         This means kiseki_client.h diverged from the Rust ffi.rs \
         (enum value, struct field, or fn signature). Update one to \
         match the other and re-run.",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
