# ADR-001: Pure Rust, No Mochi Dependency

**Status**: Accepted
**Date**: 2026-04-17
**Context**: Q-E3, A-E3

## Decision

Build all core components in Rust. Do not depend on Mochi (Mercury/Bake/SDSKV).
Learn from Mochi's design patterns (transport abstraction, composable services).

## Rationale

- Mochi has never been deployed in regulated environments (HIPAA/GDPR)
- C/C++ FFI creates a FIPS compliance surface across two languages
- Single-language FIPS module boundary is cleaner for certification
- Rust ecosystem has the building blocks (aws-lc-rs for FIPS, tokio, tonic, openraft)
- Weakest link is libfabric/CXI Rust binding — bounded scope, solvable

## Consequences

- Must build transport abstraction in Rust (kiseki-transport)
- Must build chunk storage engine in Rust (kiseki-chunk)
- Must build KV backend for log storage in Rust (RocksDB via rust-rocksdb, or sled)
- libfabric-sys crate needed for Slingshot support (immature, may need contribution)
