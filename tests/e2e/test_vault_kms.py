"""E2E: HashiCorp Vault KMS provider integration.

Tests wrap/unwrap via Vault Transit engine, key rotation, and health.
Requires Vault dev server in docker-compose (token: kiseki-e2e-token).
"""

import pytest
import requests
from tenacity import retry, stop_after_delay, wait_exponential

VAULT_URL = "http://127.0.0.1:8200"
VAULT_TOKEN = "kiseki-e2e-token"
HEADERS = {"X-Vault-Token": VAULT_TOKEN}


@retry(stop=stop_after_delay(30), wait=wait_exponential(multiplier=0.5, max=5))
def wait_for_vault():
    resp = requests.get(f"{VAULT_URL}/v1/sys/health", timeout=2)
    resp.raise_for_status()


class TestVaultKms:
    """Vault Transit engine integration tests."""

    @pytest.fixture(autouse=True)
    def _setup(self):
        try:
            wait_for_vault()
        except Exception:
            pytest.skip("Vault not available")

        # Enable transit engine (idempotent).
        requests.post(
            f"{VAULT_URL}/v1/sys/mounts/transit",
            headers=HEADERS,
            json={"type": "transit"},
        )
        # Create a test key (idempotent).
        requests.post(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test",
            headers=HEADERS,
            json={"type": "aes256-gcm96"},
        )

    def test_transit_engine_available(self):
        """Transit engine should be mounted and accessible."""
        resp = requests.get(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test",
            headers=HEADERS,
        )
        assert resp.status_code == 200
        data = resp.json()["data"]
        assert data["type"] == "aes256-gcm96"

    def test_wrap_unwrap_roundtrip(self):
        """Encrypt then decrypt via transit engine."""
        import base64

        plaintext = base64.b64encode(b"hello kiseki tenant data").decode()

        # Encrypt.
        resp = requests.post(
            f"{VAULT_URL}/v1/transit/encrypt/kiseki-tenant-test",
            headers=HEADERS,
            json={"plaintext": plaintext},
        )
        assert resp.status_code == 200
        ciphertext = resp.json()["data"]["ciphertext"]
        assert ciphertext.startswith("vault:v")

        # Decrypt.
        resp = requests.post(
            f"{VAULT_URL}/v1/transit/decrypt/kiseki-tenant-test",
            headers=HEADERS,
            json={"ciphertext": ciphertext},
        )
        assert resp.status_code == 200
        recovered = base64.b64decode(resp.json()["data"]["plaintext"])
        assert recovered == b"hello kiseki tenant data"

    def test_key_rotation(self):
        """Rotating the key should increase the version."""
        # Get current version.
        resp = requests.get(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test",
            headers=HEADERS,
        )
        version_before = resp.json()["data"]["latest_version"]

        # Rotate.
        resp = requests.post(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test/rotate",
            headers=HEADERS,
        )
        assert resp.status_code == 200 or resp.status_code == 204

        # Verify version incremented.
        resp = requests.get(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test",
            headers=HEADERS,
        )
        version_after = resp.json()["data"]["latest_version"]
        assert version_after > version_before

    def test_old_ciphertext_still_decryptable_after_rotation(self):
        """Data encrypted with old key version should still decrypt."""
        import base64

        plaintext = base64.b64encode(b"pre-rotation data").decode()

        # Encrypt with current key.
        resp = requests.post(
            f"{VAULT_URL}/v1/transit/encrypt/kiseki-tenant-test",
            headers=HEADERS,
            json={"plaintext": plaintext},
        )
        ciphertext = resp.json()["data"]["ciphertext"]

        # Rotate key.
        requests.post(
            f"{VAULT_URL}/v1/transit/keys/kiseki-tenant-test/rotate",
            headers=HEADERS,
        )

        # Old ciphertext should still decrypt.
        resp = requests.post(
            f"{VAULT_URL}/v1/transit/decrypt/kiseki-tenant-test",
            headers=HEADERS,
            json={"ciphertext": ciphertext},
        )
        assert resp.status_code == 200
        recovered = base64.b64decode(resp.json()["data"]["plaintext"])
        assert recovered == b"pre-rotation data"

    def test_vault_health_endpoint(self):
        """Vault /v1/sys/health should return 200."""
        resp = requests.get(f"{VAULT_URL}/v1/sys/health", timeout=5)
        assert resp.status_code == 200
        assert resp.json()["initialized"]
        assert not resp.json()["sealed"]
