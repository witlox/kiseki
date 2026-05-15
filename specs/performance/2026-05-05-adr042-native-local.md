# 2026-05-05 — ADR-042 native gateway data service, local matrix

**HEAD:** Phase 7 of `specs/implementation/adr-042-native-gateway.md` (TCP-framed default binding wired in `5c9ef9b`-era).
**Hardware:** dev workstation (Linux, x86_64, 16 cores) — same as the May local matrix.
**Driver:** `kiseki-profile --protocol native` (Phase 7 added the native driver). Single-node `kiseki-server` (plaintext data port; SanInterceptor falls through to the synthetic "dev" tenant). 64 KiB objects, c=16, 10 s, warmup=64.
**What changed since previous snapshot:** First end-to-end measurement of the native binding against the in-process floor (Phase 8 of the ADR-042 plan).

## Throughput

| Protocol | put-heavy | get-heavy |
|---|---:|---:|
| **InProcess (floor)** | 216 212 op/s · 13.5 GiB/s | 218 660 op/s · 13.7 GiB/s |
| **S3 HTTP** | 8 260 op/s · 516 MiB/s | 48 862 op/s · 3.05 GiB/s |
| **Native gRPC (ADR-042)** | 7 373 op/s · 461 MiB/s | **12 293 op/s · 768 MiB/s** |

## A-NG11 gate

A-NG11 commits to ≥ 80 k op/s GET, ≥ 56 k op/s PUT per node on the profile harness. This run shows:

- GET: 12 293 op/s — **15.4 % of the gate** (not cleared)
- PUT: 7 373 op/s — **13.2 % of the gate** (not cleared)

ADR-042 status remains **Proposed** until A-NG11 is satisfied. Wire shape, auth boundary, and feature surface are in place (Phases 2-6) but the gRPC tax on this single-host config is far higher than the targets allow:

- Native GET runs at **25 % of S3 HTTP GET** on the same workload. The gRPC tax should be lower than HTTP's, not higher — there is a real bottleneck on the read path.
- Native PUT runs at **89 % of S3 HTTP PUT** — close to parity, so the issue is concentrated on the read side.

## Where the GET tax lives (next-investigation candidates)

Informed-guess level without a fresh flamegraph; the `@perf @smoke` BDD scenario in `native-gateway.feature` will land the rigorous attribution once it has a step driver. Concrete suspects from code inspection:

1. **Per-call codec setup**: every call clones the channel and constructs a fresh `GatewayDataServiceClient` with `max_decoding_message_size(64 MiB)`. The codec config touches tonic-internal fields per call; a process-wide pre-built client would eliminate that. Estimated cost: 1-3 µs / call.
2. **UUID `parse_str` per request**: `OrgId` / `NamespaceId` / `CompositionId` arrive as proto `string value` fields and the handler runs `uuid::Uuid::parse_str` three times per call. The wire shape is fixed (proto3 contract) but the handler could intern parsed UUIDs in a small per-stream cache if the same tenant/namespace dominates a session. Estimated cost: ~150 ns / call — small per call but measurable at > 10 k op/s.
3. **HTTP/2 vs HTTP/1.1 framing**: tonic's HTTP/2 with `initial_stream_window_size = 16 MiB` should be FASTER than HTTP/1.1 keepalive, not slower. The 4× regression vs S3 hints at something specific to tonic's per-call work — possibly HEADERS frame compression overhead under high message rate.
4. **InterceptedService dispatch**: every native RPC pays `SanInterceptor::intercept` which reads `TlsConnectInfo` and stashes a `CanonicalSanUri` clone (the OnceLock cache landed in `5c9ef9b`, but the `req.extensions_mut().insert(...)` itself takes a TypeMap insert per call).

## Targeting Phase 9 (perf optimization slice)

The path from 12 k → 80 k op/s GET is concrete:

- Land a flamegraph capture against the harness server while the native driver is in steady-state (`KISEKI_PPROF_OUT` supports this on `--features pprof` builds).
- Audit the four candidates above against the flame.
- Iterate per-candidate, re-running the matrix.

Until that work lands, the wire-shape and security surface of ADR-042 are validated (Phases 2-6 + the Phase 7 driver) but the perf gate (A-NG11) blocks the ADR `Accepted` flip.

## Cross-references

- `specs/implementation/adr-042-native-gateway.md` — full plan, Phases 0-9.
- `specs/architecture/adr/042-native-gateway-data-service.md` — the ADR.
