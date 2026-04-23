"""E2E test: verify OpenTelemetry traces reach the Jaeger collector.

Requires docker-compose stack with Jaeger service running.
The kiseki-server must have OTEL_EXPORTER_OTLP_ENDPOINT set to
the Jaeger OTLP gRPC endpoint (http://jaeger:4317).
"""

import time

import grpc
import pytest
import requests
from tenacity import retry, stop_after_delay, wait_exponential


JAEGER_QUERY_URL = "http://127.0.0.1:16686"
KISEKI_GRPC_ADDR = "127.0.0.1:9100"
KISEKI_METRICS_ADDR = "http://127.0.0.1:9090"


@retry(stop=stop_after_delay(30), wait=wait_exponential(multiplier=0.5, max=5))
def wait_for_jaeger():
    """Wait until Jaeger UI is reachable."""
    resp = requests.get(f"{JAEGER_QUERY_URL}/api/services", timeout=2)
    resp.raise_for_status()


@retry(stop=stop_after_delay(30), wait=wait_exponential(multiplier=0.5, max=5))
def wait_for_metrics():
    """Wait until the metrics endpoint is reachable."""
    resp = requests.get(f"{KISEKI_METRICS_ADDR}/health", timeout=2)
    assert resp.status_code == 200


class TestOpenTelemetryTracing:
    """Verify distributed traces flow from kiseki-server to Jaeger."""

    @pytest.fixture(autouse=True)
    def _setup(self):
        """Wait for Jaeger and kiseki-server to be ready."""
        try:
            wait_for_jaeger()
        except Exception:
            pytest.skip("Jaeger not available — run with docker-compose")

    def test_jaeger_receives_kiseki_service(self):
        """After server boot, Jaeger should list kiseki-server as a service."""
        # Give the server time to export spans (batch interval).
        time.sleep(5)

        resp = requests.get(f"{JAEGER_QUERY_URL}/api/services", timeout=5)
        resp.raise_for_status()
        data = resp.json()
        services = data.get("data", [])
        # kiseki-server should appear (from boot-time spans).
        assert "kiseki-server" in services, (
            f"kiseki-server not found in Jaeger services: {services}"
        )

    def test_metrics_endpoint_accessible(self):
        """The /metrics endpoint should return Prometheus text format."""
        try:
            wait_for_metrics()
        except Exception:
            pytest.skip("Metrics endpoint not available")

        resp = requests.get(f"{KISEKI_METRICS_ADDR}/metrics", timeout=5)
        assert resp.status_code == 200
        assert "kiseki_" in resp.text, "metrics should contain kiseki_ prefix"

    def test_health_endpoint(self):
        """The /health endpoint should return OK."""
        try:
            wait_for_metrics()
        except Exception:
            pytest.skip("Health endpoint not available")

        resp = requests.get(f"{KISEKI_METRICS_ADDR}/health", timeout=5)
        assert resp.status_code == 200
        assert resp.text == "OK"
