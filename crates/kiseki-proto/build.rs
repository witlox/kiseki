//! Build script for `kiseki-proto`.
//!
//! Compiles the canonical `.proto` files under `specs/architecture/proto/`
//! — the single source of truth shared with `control/proto/` (Go). The
//! Rust prost/tonic output is emitted into `OUT_DIR` and included from
//! `src/lib.rs` via the standard `include!` + `tonic::include_proto!`
//! pattern.

use std::path::{Path, PathBuf};

fn main() -> std::io::Result<()> {
    // Locate the specs/architecture/proto tree relative to the workspace
    // root (CARGO_MANIFEST_DIR = .../crates/kiseki-proto).
    let manifest_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR")
        .into();
    let workspace_root = manifest_dir
        .parent() // crates/
        .and_then(Path::parent) // workspace root
        .expect("workspace root")
        .to_path_buf();
    let proto_root = workspace_root.join("specs/architecture/proto");

    // All .proto files under `kiseki/v1/`. Keep the list explicit so
    // adding a file requires a conscious update to this build script.
    let protos = [
        "kiseki/v1/common.proto",
        "kiseki/v1/log.proto",
        "kiseki/v1/chunk.proto",
        "kiseki/v1/composition.proto",
        "kiseki/v1/view.proto",
        "kiseki/v1/key.proto",
        "kiseki/v1/control.proto",
        "kiseki/v1/audit.proto",
        "kiseki/v1/advisory.proto",
    ];

    let proto_paths: Vec<PathBuf> = protos.iter().map(|p| proto_root.join(p)).collect();

    for p in &proto_paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed={}", proto_root.display());

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&proto_paths, &[proto_root])?;

    Ok(())
}
