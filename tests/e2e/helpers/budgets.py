"""Client-side time budgets, scaled per runner class.

Calibration source: release run 27322282644 (E2E Integration on a
standard 2-vCPU GitHub runner). Dev-box budgets (S3 read timeout 5 s,
no explicit gRPC deadlines) produced ReadTimeout / "timed out before
receiving SETTINGS frame" failures late in the suite — runner-
environment pressure, not semantic breaks (the same suite passes on
dev boxes, the project's ground truth).

ONE knob: ``E2E_BUDGET_SCALE`` — a float multiplier applied to every
dev-box budget below. Resolution order:

1. ``E2E_BUDGET_SCALE`` set explicitly → use it.
2. ``CI=true`` (GitHub Actions sets this on every step) → 6.0,
   so the 5 s S3 budget becomes 30 s on CI runners.
3. Otherwise (dev box) → 1.0, so local runs stay snappy.

Budgets are *upper bounds*, not sleeps: a healthy stack never spends
them, so a larger scale costs CI nothing on the happy path.
"""

from __future__ import annotations

import os


def _resolve_scale() -> float:
    raw = os.environ.get("E2E_BUDGET_SCALE", "").strip()
    if raw:
        return float(raw)
    if os.environ.get("CI", "").lower() == "true":
        return 6.0
    return 1.0


BUDGET_SCALE: float = _resolve_scale()


def scaled(seconds: float) -> float:
    """Dev-box budget × runner-class scale."""
    return seconds * BUDGET_SCALE


# --- Per-kind budgets (dev-box base values; CI default = base × 6) ---

#: Short single-object S3 HTTP ops (PUT/GET/HEAD/DELETE of small bodies).
#: 5 s dev / 30 s CI — release run 27322282644 showed 5 s is too tight
#: under a 3-node compose on a 2-vCPU runner.
S3_TIMEOUT: float = scaled(5.0)

#: Large-object S3 transfers (the 128 MiB fabric-cap pin test).
S3_LARGE_TIMEOUT: float = scaled(120.0)

#: Per-RPC deadline for gRPC health/append/read calls. Pair with
#: ``wait_for_ready=True`` so a slow HTTP/2 SETTINGS handshake under
#: accumulated runner load retries within the deadline instead of
#: failing the RPC on the first connect attempt.
GRPC_TIMEOUT: float = scaled(10.0)
