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
        "kiseki/v1/cluster_chunks.proto",
        "kiseki/v1/composition.proto",
        "kiseki/v1/view.proto",
        "kiseki/v1/key.proto",
        "kiseki/v1/control.proto",
        "kiseki/v1/audit.proto",
        "kiseki/v1/advisory.proto",
        "kiseki/v1/admin.proto",
        "kiseki/v1/storage_admin.proto",
        "kiseki/v1/gateway_data.proto",
    ];

    let proto_paths: Vec<PathBuf> = protos.iter().map(|p| proto_root.join(p)).collect();

    for p in &proto_paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed={}", proto_root.display());

    // ADR-042 §2.2 TCP-framed binding requires postcard
    // (de)serialization of the same request/response types the gRPC
    // binding ships via prost. Adding `serde::Serialize +
    // serde::Deserialize` to every kiseki.v1 type lets all bindings
    // share one type definition; the encoder swaps codecs, not types.
    //
    // Cost: generated types gain two extra trait impls. No effect on
    // gRPC binding (prost still owns the wire encode); no effect on
    // public surface (consumers see the same struct shapes).
    let mut config = tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .type_attribute(
            ".kiseki.v1",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        );
    // ADR-042 §2.2 V2 wire-format perf: postcard serializes Vec<u8>
    // via serde's default `serialize_seq` (one byte at a time).
    // For bulk-payload fields like `GetObjectResponse.data` (64 KiB)
    // that's 65 K calls to AllocVec::try_push per response — 84% of
    // CPU at 64 KiB GET per the staircase flamegraph. Adding
    // `#[serde(with = "serde_bytes")]` switches the path to
    // `serialize_bytes` which postcard handles as a single
    // bulk-memcopy. The Rust type stays `Vec<u8>` so no caller
    // needs to change.
    let bulk_byte_fields = &[
        // Hot bulk paths — perf-critical.
        ".kiseki.v1.native.PutObjectRequest.data",
        ".kiseki.v1.native.GetObjectResponse.data",
        ".kiseki.v1.native.WriteRequest.data",
        ".kiseki.v1.native.ReadResponse.data",
        // Streaming chunk variants (oneof inner messages).
        ".kiseki.v1.native.PutObjectChunk.Data.data",
        ".kiseki.v1.native.GetObjectChunk.Data.data",
        ".kiseki.v1.native.PutPartChunk.Data.data",
        ".kiseki.v1.native.WriteChunk.Data.data",
        ".kiseki.v1.native.ReadChunk.Data.data",
        // Encryption payloads.
        ".kiseki.v1.native.SealedChunk.ciphertext",
        // DEK ticket — short but frequent. (BatchFetchDekRequest's
        // `dek_fetch_tickets` is `Vec<Vec<u8>>`; serde_bytes doesn't
        // handle nested Vec without a custom `with` module — left at
        // default seq encoding for now, the per-call cost is small
        // since each ticket is < 256 bytes.)
        ".kiseki.v1.native.FetchDekRequest.dek_fetch_ticket",
    ];
    for field in bulk_byte_fields {
        config = config.field_attribute(field, r#"#[serde(with = "serde_bytes")]"#);
    }
    config.compile_protos(&proto_paths, &[proto_root])?;

    Ok(())
}
