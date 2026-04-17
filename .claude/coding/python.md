# Kiseki — Python Coding Standards

Extends `.claude/guidelines/python.md` with project-specific conventions.

## Scope

Python in kiseki serves two purposes:
1. **PyO3 client bindings** — thin Python wrapper around the Rust native client
2. **E2E test scripting** — validation, integration testing, scenario orchestration

Python is NOT used for any data-path or performance-critical code.

## PyO3 Bindings

### Package

```
bindings/python/
├── pyproject.toml          # maturin build config
├── src/
│   └── lib.rs              # PyO3 module definition (Rust side)
├── kiseki/
│   ├── __init__.py         # Re-export from native module
│   ├── _native.pyi         # Type stubs for the Rust-generated module
│   ├── client.py           # Pythonic wrapper around native client
│   └── types.py            # Python dataclasses mirroring Rust types
└── tests/
    └── test_client.py
```

### Conventions

- Build via `maturin` (PyO3 build tool)
- Type stubs (`.pyi`) maintained alongside the Rust module
- Pythonic API: convert Rust Result → Python exceptions, Rust Vec → list, etc.
- Domain types from `specs/ubiquitous-language.md` as Python dataclasses
- No business logic in Python — delegate to Rust via PyO3

### Example pattern

```python
# kiseki/client.py — Pythonic wrapper
from kiseki._native import NativeClient as _NativeClient
from kiseki.types import Composition, Namespace

class KisekiClient:
    """High-level Python client for Kiseki."""

    def __init__(self, seeds: list[str], tenant_cert: str, tenant_key: str) -> None:
        self._inner = _NativeClient(seeds, tenant_cert, tenant_key)

    def read(self, namespace: str, path: str, offset: int = 0, length: int = -1) -> bytes:
        """Read data from a Kiseki composition."""
        return self._inner.read(namespace, path, offset, length)

    def write(self, namespace: str, path: str, data: bytes) -> None:
        """Write data to a Kiseki composition."""
        self._inner.write(namespace, path, data)
```

## E2E Test Scripting

### Package

```
tests/e2e/
├── conftest.py             # Cluster fixtures (start/stop, tenant setup)
├── test_write_read.py      # End-to-end write → read via multiple protocols
├── test_encryption.py      # Verify no plaintext leaks
├── test_crypto_shred.py    # KEK destruction → data unreadable
├── test_multiprotocol.py   # Write NFS, read S3 (cross-view consistency)
├── test_failover.py        # Node failure → client reconnects
├── test_federation.py      # Cross-site replication
├── helpers/
│   ├── cluster.py          # Cluster lifecycle management
│   ├── nfs.py              # NFS mount/unmount helpers
│   ├── s3.py               # boto3 S3 client helpers
│   └── assertions.py       # Custom assertions (no plaintext in capture, etc.)
└── fixtures/
    └── sample_data.py      # Test data generators
```

### Conventions

- Python 3.11+ (no backward compatibility needed for test tooling)
- `pytest` with markers: `@pytest.mark.e2e`, `@pytest.mark.slow`
- Cluster managed via fixtures (start kiseki-server processes, provision tenants)
- Three client paths tested: native (PyO3), NFS (mount + POSIX ops), S3 (boto3)
- Every e2e test verifies encryption invariants:
  - No plaintext on disk (inspect chunk files)
  - No plaintext on wire (packet capture analysis)
  - Crypto-shred makes data unreadable
- `hypothesis` for property-based testing of dedup, refcount, versioning
- `tenacity` for retry-with-backoff when waiting for async operations
  (view materialization, federation sync)

### E2E test patterns

```python
# test_write_read.py
import pytest
from kiseki import KisekiClient

@pytest.mark.e2e
def test_write_via_native_read_via_s3(cluster, tenant_pharma, s3_client):
    """Write through native client, read back through S3 gateway."""
    client = KisekiClient(
        seeds=cluster.seed_endpoints,
        tenant_cert=tenant_pharma.cert_path,
        tenant_key=tenant_pharma.key_path,
    )

    # Write via native client
    data = b"checkpoint data " * 1000
    client.write("trials", "/results/epoch-100.pt", data)

    # Read via S3 (may have staleness — retry within bound)
    @tenacity.retry(stop=tenacity.stop_after_delay(5))
    def read_s3():
        obj = s3_client.get_object(Bucket="trials", Key="results/epoch-100.pt")
        return obj["Body"].read()

    assert read_s3() == data
```

### What e2e tests cover (maps to Gherkin features)

| Test file | Covers | Feature files |
|---|---|---|
| `test_write_read.py` | Write/read round-trip all protocols | log, chunk-storage, composition, gateway, client |
| `test_encryption.py` | No plaintext at rest or in flight | key-management, chunk-storage |
| `test_crypto_shred.py` | KEK destroy → data unreadable → GC | key-management, control-plane |
| `test_multiprotocol.py` | Write NFS, read S3 (cross-view) | view-materialization, gateway |
| `test_failover.py` | Node crash → client reconnects | operational, native-client |
| `test_federation.py` | Cross-site config sync, data replication | control-plane |

## Dependencies

### PyO3 bindings

```toml
[build-system]
requires = ["maturin>=1.0"]
build-backend = "maturin"

[project]
name = "kiseki"
requires-python = ">=3.11"

[project.optional-dependencies]
dev = ["pytest", "pytest-cov", "mypy", "ruff", "black"]
```

### E2E test tooling

```toml
[project.optional-dependencies]
e2e = [
    "pytest",
    "pytest-cov",
    "pytest-asyncio",
    "hypothesis",
    "tenacity",
    "boto3",           # S3 client
    "structlog",       # Structured logging in test output
    "pydantic",        # Test config validation
]
```
