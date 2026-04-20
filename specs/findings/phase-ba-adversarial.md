# Phase B+A Adversarial Review (E2E + BDD Harness)

**Date**: 2026-04-20. **Reviewer**: Adversary role.

## CRITICAL (3)

### BA-ADV-1: Dockerfile uses rust:latest — non-deterministic builds
- **Location**: `Dockerfile.server:1,11`
- **Issue**: `FROM rust:latest` changes over time — unreproducible builds
- **Resolution**: Pin to specific version digest

### BA-ADV-2: Session-scoped fixture allows cross-test contamination
- **Location**: `tests/e2e/conftest.py:18-23`
- **Issue**: All tests share one server. Maintenance mode test can leave shard locked if it fails mid-test. State accumulates.
- **Resolution**: Reset shard state in teardown, or use per-module scope

### BA-ADV-3: Cross-protocol test doesn't isolate test-specific delta
- **Location**: `tests/e2e/test_cross_protocol.py:39-47`
- **Issue**: Reads all deltas from seq 1-1000, asserts "latest is Create" — could match prior test's delta
- **Resolution**: Use unique hashed_key per test, verify in assertion

## HIGH (4)

### BA-ADV-4: Docker compose failure not diagnosed
- **Location**: `tests/e2e/helpers/cluster.py:50-70`
- **Issue**: `capture_output=True` silently drops stderr. If build fails, test hangs 60s with no message.
- **Resolution**: Log stderr on failure, check compose logs before wait

### BA-ADV-5: poll_views() silently no-ops if view not tracked
- **Location**: `crates/kiseki-acceptance/tests/acceptance.rs:189-196`
- **Issue**: If no views in `view_ids`, poll does nothing. Then steps pass with zero deltas consumed.
- **Resolution**: Assert poll result > 0 in pipeline-relevant steps

### BA-ADV-6: Proto stubs committed with no freshness check
- **Location**: `tests/e2e/generate_proto.sh`, `tests/e2e/proto/`
- **Issue**: Proto changes require manual regeneration. No CI check.
- **Resolution**: Add CI step: generate + git diff --exit-code

### BA-ADV-7: KISEKI_BOOTSTRAP defaults to false silently
- **Location**: `crates/kiseki-server/src/config.rs:76-78`
- **Issue**: Without KISEKI_BOOTSTRAP=true, gateways have no namespaces. All S3/NFS requests fail 404.
- **Resolution**: Log warning when bootstrap not set

## MEDIUM (5)

### BA-ADV-8: BDD cross-context assertions don't filter by composition ID
- **Location**: `crates/kiseki-acceptance/tests/steps/composition.rs:36-51`
- **Issue**: Checks "any Create delta exists" not "this composition's delta"
- **Resolution**: Match delta payload against composition ID bytes

### BA-ADV-9: gRPC channel doesn't detect server crash
- **Location**: `tests/e2e/conftest.py:26-31`
- **Issue**: Session-scoped channel stays open if server dies mid-test
- **Resolution**: Add healthcheck before each test or per-function fixture

### BA-ADV-10: Docker healthcheck always succeeds
- **Location**: `docker-compose.yml:25-29`
- **Issue**: `test: ["CMD-SHELL", "true"]` — always returns 0
- **Resolution**: Use grpc_health_probe

### BA-ADV-11: Maintenance test doesn't verify shard state via ShardHealth
- **Location**: `tests/e2e/test_log_roundtrip.py:67-110`
- **Issue**: Doesn't check shard state after enable/disable maintenance
- **Resolution**: Add ShardHealth assertion between maintenance changes

### BA-ADV-12: Broad #![allow] in acceptance harness suppresses useful warnings
- **Location**: `crates/kiseki-acceptance/tests/acceptance.rs:8-16`
- **Issue**: unused_imports, dead_code suppressed globally
- **Resolution**: Fix underlying warnings, remove broad allows

## LOW (2)

### BA-ADV-13: ETag uniqueness not verified in multi-object test
- **Location**: `tests/e2e/test_cross_protocol.py:94-110`
- **Resolution**: Add `assert len(set(etags.values())) == 5`

### BA-ADV-14: sys.path insertion fragile
- **Location**: `tests/e2e/conftest.py:13`
- **Resolution**: Add existence check for proto dir
