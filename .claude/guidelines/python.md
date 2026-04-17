# Python Guidelines

## Version & Tooling

- Python 3.11+ for new projects, 3.9+ where broader compatibility needed
- Build system: `hatchling` or `setuptools` via `pyproject.toml` (no setup.py)
- Format: `black` (line-length 88)
- Lint: `ruff` (replaces flake8, isort, pyupgrade, bugbear)
- Type check: `mypy` with `disallow_untyped_defs = true`
- Security: `bandit`
- Dead code: `vulture`
- Pre-commit: `pre-commit` framework with ruff, black, mypy hooks

## Style

- Type hints required on all public functions and methods
- Modern syntax: `dict[str, Any]` not `Dict`, `X | None` not `Optional[X]`
- Google-style docstrings for public APIs (Args, Returns, Raises)
- Line length: 88 characters (enforced by black + ruff)
- Imports grouped: stdlib → external → internal (enforced by ruff rule I)
- No `type: ignore` without explanation; prefer fixing the type issue

## Ruff Configuration

```toml
[tool.ruff]
line-length = 88

[tool.ruff.lint]
select = [
    "E",   # pycodestyle errors
    "W",   # pycodestyle warnings
    "F",   # pyflakes
    "I",   # isort
    "C",   # flake8-comprehensions
    "B",   # flake8-bugbear
    "UP",  # pyupgrade
]
ignore = [
    "E501",  # line too long (handled by black)
    "B008",  # function calls in argument defaults
]

[tool.ruff.lint.per-file-ignores]
"__init__.py" = ["F401"]
"tests/*" = ["F401", "F811"]
```

## MyPy Configuration

```toml
[tool.mypy]
disallow_untyped_defs = true
disallow_incomplete_defs = true
check_untyped_defs = true
no_implicit_optional = true
warn_redundant_casts = true
warn_no_return = true
strict_equality = true
```

## Testing

- Framework: `pytest` with `pytest-cov`, `pytest-asyncio`, `pytest-mock`
- Async: `asyncio_mode = "auto"` in pytest config
- Property-based: `hypothesis` for invariant/fuzz testing
- Markers: `@pytest.mark.slow`, `@pytest.mark.integration`, `@pytest.mark.unit`
- Integration tests skipped by default; enabled via env var
- Coverage: 60% minimum enforced (`fail_under`), 80%+ target for new code
- Fixtures in `conftest.py`; reusable sample data as fixtures

## Pytest Configuration

```toml
[tool.pytest.ini_options]
testpaths = ["tests"]
python_files = ["test_*.py"]
python_classes = ["Test*"]
python_functions = ["test_*"]
addopts = ["-ra", "--strict-markers", "--strict-config", "--cov", "--cov-report=term-missing"]
markers = [
    "slow: marks tests as slow",
    "integration: marks tests as integration tests",
    "unit: marks tests as unit tests",
]
```

## Build System (Makefile targets)

```makefile
dev:              pip install -e ".[dev]"
test:             pytest tests/ -v
test-cov:         pytest with coverage report
test-integration: integration tests (requires Docker)
lint:             ruff check + mypy
format:           black + ruff --fix
clean:            remove build artifacts, __pycache__, .coverage
```

## Pre-commit Hooks

```yaml
repos:
  - repo: https://github.com/astral-sh/ruff-pre-commit
    hooks:
      - id: ruff
        args: [--fix, --exit-non-zero-on-fix]

  - repo: https://github.com/psf/black
    hooks:
      - id: black

  - repo: https://github.com/pre-commit/mirrors-mypy
    hooks:
      - id: mypy
        args: [--ignore-missing-imports]
```

## CI Pipeline

Same three-stage pattern as Go/Rust:
1. Build — `python -m build`, upload dist/ artifacts
2. Validate — ruff, black --check, mypy, bandit, vulture
3. Test — pytest with coverage → Codecov, threshold enforcement

## Docker

- Multi-stage: `python:<version>-slim` builder → runtime
- Non-root user in runtime image
- Dev dependencies NOT in runtime image
- Test compose with tmpfs for speed

## Patterns

- `pydantic` for configuration and data validation at boundaries
- `structlog` for structured logging
- `tenacity` for retry logic with exponential backoff
- `click` for CLI interfaces
- `FastAPI` + `uvicorn` for REST APIs (where applicable)
