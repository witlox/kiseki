# Open Adversarial Findings Index

Generated: 2026-04-19. Total: 67 open findings across 21 documents.

Grouped by what blocks resolution.

## Blocked by: Raft multi-node (need B.3/B.4 Raft instantiation)

- phase3-gate2: No Raft integration (High, deferred)
- phase4-gate2: No Raft replication for master keys (High, deferred)
- phase5-gate2: No persistence or Raft for audit (High, deferred)
- b1-raft-scaffold-gate: No Raft transport proto (Medium)
- b2-key-raft-gate: Key material plaintext in Raft log (High, documented)
- b2-key-raft-gate: Raft not wired for log/audit (Medium)
- b3b4-raft-gate: Log state machine tracks counts not deltas (Medium)

## Blocked by: Cross-context integration (R3)

- phase7-gate2: No refcount integration with chunk store (High)
- phase7-gate2: Multipart abort doesn't clean up chunks (Medium)

## Blocked by: gRPC wiring (R5)

- wi3-gate: No mTLS interceptor on gRPC services (Medium)
- wi3-gate: Go ControlService not wired (Medium)
- wi3-gate: Advisory streaming RPCs unimplemented (Low)
- a4-mtls-grpc-gate: No integration test with mTLS client (Medium)
- phase12-gate2: Server is scaffold, no e2e test (High)

## Blocked by: Protocol implementations (R7)

- phase9-gate2: No implementation behind GatewayOps (Medium)
- phase10-gate2: No FUSE implementation (Medium)
- phase10-gate2: Discovery response not authenticated (Medium)

## Blocked by: Missing features

- phase2-gate2: OrgId extraction placeholder (Medium) — RESOLVED in A.2
- phase2-gate2: No connection timeout (Medium) — RESOLVED in A.1
- phase2-gate2: No CRL checking (Medium) — RESOLVED in A.3 (untested)
- phase2-gate2: No connection pool (Low)
- phase2-gate2: No SPIFFE SAN parsing (Low) — RESOLVED in A.2
- phase3-gate2: Naive split midpoint (Medium)
- phase3-gate2: Rough byte_size estimate (Low)
- phase3-gate2: gc_floor not exposed (Low)
- phase6-gate2: No EC encoding (Medium)
- phase6-gate2: No placement engine (Medium)
- phase11-gate2: No persistence or gRPC server for control plane (High)
- phase11-gate2: AccessRequest uses time.Now() (Medium)
- phase11_5-gate2: No gRPC server or isolated runtime (High)
- phase11_5-gate2: No arc-swap for AdvisoryLookup (Medium)
- phase11_5-gate2: No k-anonymity telemetry (Medium)

## Accepted risk / by design

- phase0-gate2: Proto ChunkId allows variable-length (Medium, conversion layer)
- phase0-gate2: Proto nonce/auth_tag no length validation (Medium, conversion layer)
- phase0-gate2: Unbounded String fields (Low, proto boundary)
- phase1-gate2: Key material stack copies not zeroized (Medium, Rust limitation)
- phase1-gate2: HKDF info string lacks epoch (Low, defense-in-depth)
- phase1-gate2: Envelope fields all public (Low, deferred)
- phase10-gate2: Cache stores plaintext (Low, accepted per spec)
- a1-timeout-gate: No TCP keepalive/NODELAY (Low)
- a2-x509-gate: Fingerprint fallback still reachable (Low)
- a2-x509-gate: Multi-valued OU not handled (Low)
- a3-crl-gate: CRL loaded once at startup, no refresh (Low)
- a4-mtls-grpc-gate: Plaintext fallback for dev (Medium)
- a4-mtls-grpc-gate: CRL not in tonic path (Low)
- a5-shutdown-gate: Both runtimes listen for ctrl_c independently (Low)
- b5-bootstrap-gate: Error handling discards Raft details (Low)
- b5-bootstrap-gate: No shutdown handling (Low)
