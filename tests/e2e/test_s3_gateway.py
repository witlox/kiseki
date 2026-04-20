"""E2E: S3 gateway PUT/GET/HEAD/DELETE via HTTP."""

from __future__ import annotations

import pytest
import requests

from helpers.cluster import ServerInfo


S3_BASE = "http://127.0.0.1:9000"


@pytest.mark.e2e
def test_s3_put_and_get(kiseki_server: ServerInfo) -> None:
    """PUT an object via S3, GET it back, verify roundtrip."""
    data = b"hello from S3 e2e test"

    # PUT
    put_resp = requests.put(f"{S3_BASE}/default/testkey", data=data, timeout=5)
    assert put_resp.status_code == 200, f"PUT failed: {put_resp.text}"
    etag = put_resp.headers.get("etag", "").strip('"')
    assert etag, "expected ETag in response"

    # GET (use etag as composition ID key)
    get_resp = requests.get(f"{S3_BASE}/default/{etag}", timeout=5)
    assert get_resp.status_code == 200, f"GET failed: {get_resp.text}"
    assert get_resp.content == data


@pytest.mark.e2e
def test_s3_head(kiseki_server: ServerInfo) -> None:
    """PUT then HEAD — verify content-length."""
    data = b"head test data 1234567890"

    put_resp = requests.put(f"{S3_BASE}/default/headkey", data=data, timeout=5)
    etag = put_resp.headers.get("etag", "").strip('"')

    head_resp = requests.head(f"{S3_BASE}/default/{etag}", timeout=5)
    assert head_resp.status_code == 200
    assert int(head_resp.headers.get("content-length", 0)) == len(data)


@pytest.mark.e2e
def test_s3_get_not_found(kiseki_server: ServerInfo) -> None:
    """GET non-existent object returns 404."""
    resp = requests.get(
        f"{S3_BASE}/default/00000000-0000-0000-0000-000000000099",
        timeout=5,
    )
    assert resp.status_code == 404


@pytest.mark.e2e
def test_s3_delete(kiseki_server: ServerInfo) -> None:
    """DELETE returns 204 (no-op for now)."""
    resp = requests.delete(f"{S3_BASE}/default/anything", timeout=5)
    assert resp.status_code == 204
