//! Build script for `kiseki-fuse-sys`.
//!
//! Locates libfuse3 via pkg-config, emits the cargo link directives,
//! and runs bindgen against `wrapper.h` to generate Rust bindings
//! into `$OUT_DIR/bindings.rs`. The generated file is `include!`'d
//! from `src/lib.rs`.
//!
//! Failure to find libfuse3 panics with the operator-friendly
//! error message specified in `specs/implementation/libfuse-swap.md`
//! §"crates/kiseki-fuse-sys/" — names every supported distro's
//! install command so the contributor doesn't have to hunt for it.

use std::env;
use std::path::PathBuf;

fn main() {
    // 1. pkg-config: locate libfuse3 + emit `cargo:rustc-link-lib=fuse3`
    //    and the `cargo:rustc-link-search=...` paths.
    //
    //    `print_system_libs(false)`: don't emit `cargo:rustc-link-lib`
    //    for system libs we don't need (libpthread, libdl etc.)
    //    pkg-config would otherwise drag in.
    //
    //    `cargo_metadata(true)`: turn pkg-config output into the
    //    cargo:rustc-link-lib / cargo:rustc-link-search / cargo:include
    //    metadata cargo expects.
    let lib = match pkg_config::Config::new()
        .atleast_version("3.10")
        .print_system_libs(false)
        .cargo_metadata(true)
        .probe("fuse3")
    {
        Ok(lib) => lib,
        Err(e) => {
            // Operator-friendly error per ADR-043 implementation plan.
            // Names every supported distro's install command so a
            // contributor hitting this for the first time has the
            // fix in hand.
            panic!(
                "kiseki-fuse-sys: libfuse3 development headers not found via pkg-config.\n\
                 \n\
                 pkg-config error: {e}\n\
                 \n\
                 Install:\n  \
                   Debian/Ubuntu: apt-get install libfuse3-dev\n  \
                   RHEL/Fedora:   dnf install fuse3-devel\n  \
                   Arch:          pacman -S fuse3\n  \
                   macOS:         not supported (libfuse3 is Linux-only; see ADR-043 §\"Platform scope\")\n\
                 \n\
                 The `fuse` feature on kiseki-client requires this. \
                 To build without FUSE support, omit the feature.\n",
            );
        }
    };

    // 2. Bindgen: generate Rust bindings against wrapper.h.
    //    Include paths come from pkg-config (libfuse3-dev installs
    //    to /usr/include/fuse3 on Debian, /usr/include/fuse3 on RHEL).
    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        // Re-run if the wrapper changes.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Emit `core::ffi::*` types (default in newer bindgen) so
        // `unsafe extern "C"` signatures match Rust's std types.
        .ctypes_prefix("::core::ffi")
        // Block macros that pull in problematic platform-specific
        // shapes; we don't use `__GNUC_PREREQ` etc. and bindgen sometimes
        // generates warnings for them.
        .blocklist_item("FP_.*")
        .blocklist_item("FE_.*")
        // Only allow items inside the FUSE prefix surface — keeps the
        // generated output small and avoids pulling in every type
        // libc happens to expose through fuse_common.h.
        .allowlist_function("fuse_.*")
        .allowlist_type("fuse_.*|FUSE_.*")
        .allowlist_var("FUSE_.*|fuse_.*")
        // libfuse 3.10–3.16 expose `fuse_session_new` as a real
        // function; libfuse 3.17+ replaced it with a versioning
        // macro that expands to `fuse_session_new_versioned`. To
        // keep the kiseki-fuse-sys API surface stable across both
        // versions, we blocklist whichever shape bindgen sees
        // (function on ≤ 3.16, nothing on 3.17+) and provide a
        // single hand-written `extern "C"` declaration in `src/lib.rs`
        // tied to the `@@FUSE_3.0` symbol exported by every libfuse
        // 3.x release. Without this blocklist, building on 3.10 fails
        // with E0428 "the name `fuse_session_new` is defined twice".
        .blocklist_function("fuse_session_new");

    // Add include paths pkg-config gave us.
    for include_path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", include_path.display()));
    }

    let bindings = builder
        .generate()
        .expect("kiseki-fuse-sys: bindgen failed to generate bindings against wrapper.h");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set by cargo"));
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("kiseki-fuse-sys: failed to write generated bindings to OUT_DIR/bindings.rs");

    // 3. Re-run if wrapper.h or build.rs changes.
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
}
