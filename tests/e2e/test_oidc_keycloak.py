"""E2E: Keycloak OIDC provider integration.

Tests real JWT issuance from Keycloak, JWKS endpoint availability,
and token claim extraction.
Requires Keycloak in docker-compose (admin/admin).
"""

import json
import time

import pytest
import requests
from tenacity import retry, stop_after_delay, wait_exponential

KEYCLOAK_URL = "http://127.0.0.1:8080"
ADMIN_USER = "admin"
ADMIN_PASS = "admin"
REALM = "kiseki-test"


@retry(stop=stop_after_delay(120), wait=wait_exponential(multiplier=1, max=10))
def wait_for_keycloak():
    resp = requests.get(f"{KEYCLOAK_URL}/health/ready", timeout=5)
    if resp.status_code != 200:
        raise ConnectionError("Keycloak not ready")


def get_admin_token():
    """Get Keycloak admin access token."""
    resp = requests.post(
        f"{KEYCLOAK_URL}/realms/master/protocol/openid-connect/token",
        data={
            "grant_type": "client_credentials",
            "client_id": "admin-cli",
            "username": ADMIN_USER,
            "password": ADMIN_PASS,
            "grant_type": "password",
        },
    )
    resp.raise_for_status()
    return resp.json()["access_token"]


def ensure_realm(admin_token):
    """Create test realm if it doesn't exist."""
    headers = {"Authorization": f"Bearer {admin_token}"}
    resp = requests.get(f"{KEYCLOAK_URL}/admin/realms/{REALM}", headers=headers)
    if resp.status_code == 404:
        requests.post(
            f"{KEYCLOAK_URL}/admin/realms",
            headers={**headers, "Content-Type": "application/json"},
            json={
                "realm": REALM,
                "enabled": True,
                "registrationAllowed": False,
            },
        ).raise_for_status()


def ensure_client(admin_token):
    """Create a test client in the realm."""
    headers = {
        "Authorization": f"Bearer {admin_token}",
        "Content-Type": "application/json",
    }
    # Check if client exists.
    resp = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients?clientId=kiseki-client",
        headers=headers,
    )
    clients = resp.json()
    if not clients:
        requests.post(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/clients",
            headers=headers,
            json={
                "clientId": "kiseki-client",
                "enabled": True,
                "directAccessGrantsEnabled": True,
                "serviceAccountsEnabled": True,
                "clientAuthenticatorType": "client-secret",
                "secret": "kiseki-test-secret",
                "protocol": "openid-connect",
                "publicClient": False,
            },
        ).raise_for_status()


def ensure_user(admin_token):
    """Create a test user."""
    headers = {
        "Authorization": f"Bearer {admin_token}",
        "Content-Type": "application/json",
    }
    resp = requests.get(
        f"{KEYCLOAK_URL}/admin/realms/{REALM}/users?username=testuser",
        headers=headers,
    )
    if not resp.json():
        requests.post(
            f"{KEYCLOAK_URL}/admin/realms/{REALM}/users",
            headers=headers,
            json={
                "username": "testuser",
                "enabled": True,
                "credentials": [{"type": "password", "value": "testpass", "temporary": False}],
                "attributes": {
                    "org": ["test-org-123"],
                    "project": ["ml-training"],
                },
            },
        ).raise_for_status()


class TestOidcKeycloak:
    """Keycloak OIDC integration tests."""

    @pytest.fixture(autouse=True)
    def _setup(self):
        try:
            wait_for_keycloak()
        except Exception:
            pytest.skip("Keycloak not available")

        try:
            token = get_admin_token()
            ensure_realm(token)
            ensure_client(token)
            ensure_user(token)
        except Exception as e:
            pytest.skip(f"Keycloak setup failed: {e}")

    def test_jwks_endpoint_available(self):
        """JWKS endpoint should return signing keys."""
        resp = requests.get(
            f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/certs",
            timeout=5,
        )
        assert resp.status_code == 200
        jwks = resp.json()
        assert "keys" in jwks
        assert len(jwks["keys"]) > 0
        # At least one RSA key.
        rsa_keys = [k for k in jwks["keys"] if k.get("kty") == "RSA"]
        assert len(rsa_keys) > 0, "JWKS should contain at least one RSA key"

    def test_openid_configuration_discovery(self):
        """OpenID Connect discovery endpoint should work."""
        resp = requests.get(
            f"{KEYCLOAK_URL}/realms/{REALM}/.well-known/openid-configuration",
            timeout=5,
        )
        assert resp.status_code == 200
        config = resp.json()
        assert config["issuer"] == f"{KEYCLOAK_URL}/realms/{REALM}"
        assert "token_endpoint" in config
        assert "jwks_uri" in config
        assert "authorization_endpoint" in config

    def test_issue_jwt_and_validate_claims(self):
        """Issue a real JWT from Keycloak and verify claims."""
        # Get a token via resource owner password grant.
        resp = requests.post(
            f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/token",
            data={
                "grant_type": "password",
                "client_id": "kiseki-client",
                "client_secret": "kiseki-test-secret",
                "username": "testuser",
                "password": "testpass",
            },
        )
        assert resp.status_code == 200, f"Token request failed: {resp.text}"
        token_data = resp.json()

        assert "access_token" in token_data
        access_token = token_data["access_token"]

        # Decode JWT payload (base64url, no verification — just check structure).
        import base64

        parts = access_token.split(".")
        assert len(parts) == 3, "JWT should have 3 parts"

        # Decode payload.
        payload = parts[1]
        # Add padding.
        payload += "=" * (4 - len(payload) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))

        assert claims["iss"] == f"{KEYCLOAK_URL}/realms/{REALM}"
        assert claims["preferred_username"] == "testuser"
        assert "exp" in claims
        assert claims["exp"] > time.time(), "token should not be expired"

    def test_expired_token_rejected_by_keycloak(self):
        """A request with an expired token should be rejected."""
        # Use a clearly expired token.
        expired_token = "eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjB9.invalid"
        resp = requests.get(
            f"{KEYCLOAK_URL}/realms/{REALM}/protocol/openid-connect/userinfo",
            headers={"Authorization": f"Bearer {expired_token}"},
        )
        assert resp.status_code in (401, 403), "expired token should be rejected"
