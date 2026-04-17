Run end-to-end tests. Use after integration is functional.

Prerequisites:
- kiseki-server binary built and runnable
- kiseki-keyserver binary built and runnable
- kiseki-control binary built and runnable
- Python e2e dependencies installed: `pip install -e ".[e2e]"` from bindings/python/

Steps:

1. Check binaries exist:
   - `cargo build --release -p kiseki-server`
   - `cargo build --release -p kiseki-keyserver`
   - `cd control && go build ./cmd/kiseki-control/`

2. Start test cluster (3 storage nodes, 1 key server, 1 control plane):
   - Use `tests/e2e/helpers/cluster.py` or docker-compose if available

3. Run e2e tests:
   - `pytest tests/e2e/ -v --tb=short -m e2e`
   - Skip slow tests: `pytest tests/e2e/ -v -m "e2e and not slow"`

4. Run with encryption validation:
   - `pytest tests/e2e/test_encryption.py -v` — verify no plaintext leaks

5. Report:
   - Pass/fail per test file
   - Any encryption invariant violations
   - Cluster logs for failed tests

If ANY e2e test fails, investigate before declaring integration complete.
