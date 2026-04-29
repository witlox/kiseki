"""E2E: S3 gateway PUT/GET/HEAD/DELETE via HTTP."""

from __future__ import annotations

import pytest
import requests

from helpers.cluster import ServerInfo, start_cluster, stop_cluster


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


@pytest.mark.e2e
def test_s3_large_put_exceeds_64mib_fabric_cap() -> None:
    """A 128 MiB PUT must succeed against the 3-node cluster.

    The gateway stores each S3 PUT as one envelope; that envelope
    rides in a single PutFragment gRPC message during cross-node
    fan-out. Tonic's default gRPC cap is 4 MiB, kiseki's
    `FABRIC_MAX_MESSAGE_BYTES` lifts that to 256 MiB so PUTs in
    the 64-200 MiB range go through. Without the lift, this test
    fails with HTTP 500 + `quorum lost: only 1/2 replicas acked`
    (the receiver rejects the oversized PutFragment, fan-out
    quorum collapses to leader-only, the leader bails).

    This test pins the cap by exercising it: a 128 MiB PUT
    necessarily exceeds 64 MiB, so any future regression that
    drops the cap below ~135 MiB (128 MiB + protobuf framing +
    crypto envelope overhead) will fail this test loudly.

    Uses the 3-node compose directly (the cap only matters when
    cluster fan-out runs); a single-node cluster never invokes
    PutFragment, so this can't piggyback on `kiseki_server`.
    """
    cluster = start_cluster()
    try:
        # 128 MiB > 64 MiB cap: the bump is necessary for this PUT.
        # Random-ish content so dedup doesn't trivialize the test.
        payload = bytes((i * 17) & 0xFF for i in range(128 * 1024 * 1024))
        s3 = f"http://{cluster.nodes[0].data_addr.split(':')[0]}:9000"

        put = requests.put(f"{s3}/default/large-128m", data=payload, timeout=120)
        put.raise_for_status()
        etag = put.headers.get("etag", "").strip('"')
        assert etag, "PUT must return an etag"

        # GET the same bytes back. Single-stream GET of 128 MiB
        # also exercises the read-side fabric path (chunk fetch
        # from any peer when the local store cold-misses).
        get = requests.get(f"{s3}/default/{etag}", timeout=120)
        get.raise_for_status()
        assert len(get.content) == len(payload), (
            f"GET length {len(get.content)} != PUT length {len(payload)}"
        )
        assert get.content == payload, "GET content mismatched the PUT payload"
    finally:
        stop_cluster(cluster)
