"""E2E: S3 API operations via HTTP.

Tests bucket CRUD, object put/get/delete, and list operations
against the kiseki S3 gateway.
"""

import pytest
import requests
from tenacity import retry, stop_after_delay, wait_exponential

S3_URL = "http://127.0.0.1:9000"


@retry(stop=stop_after_delay(30), wait=wait_exponential(multiplier=0.5, max=5))
def wait_for_s3():
    # S3 gateway doesn't have a /health, just try a GET /.
    resp = requests.get(S3_URL, timeout=2)
    # 200 = list buckets works (even if empty).
    if resp.status_code not in (200, 404):
        raise ConnectionError(f"S3 not ready: {resp.status_code}")


class TestS3BucketCrud:
    """S3 bucket create/head/list/delete operations."""

    @pytest.fixture(autouse=True)
    def _setup(self):
        try:
            wait_for_s3()
        except Exception:
            pytest.skip("S3 gateway not available")

    def test_create_bucket(self):
        """PUT /<bucket> creates a bucket."""
        resp = requests.put(f"{S3_URL}/e2e-test-bucket")
        assert resp.status_code == 200

    def test_create_duplicate_bucket_returns_409(self):
        """PUT /<bucket> for an existing bucket returns 409."""
        requests.put(f"{S3_URL}/dup-test-bucket")
        resp = requests.put(f"{S3_URL}/dup-test-bucket")
        assert resp.status_code == 409

    def test_head_existing_bucket(self):
        """HEAD /<bucket> returns 200 for existing bucket."""
        requests.put(f"{S3_URL}/head-test-bucket")
        resp = requests.head(f"{S3_URL}/head-test-bucket")
        assert resp.status_code == 200

    def test_head_nonexistent_bucket_returns_404(self):
        """HEAD /<bucket> returns 404 for missing bucket."""
        resp = requests.head(f"{S3_URL}/nonexistent-bucket-xyz")
        assert resp.status_code == 404

    def test_delete_bucket(self):
        """DELETE /<bucket> removes the bucket."""
        requests.put(f"{S3_URL}/delete-test-bucket")
        resp = requests.delete(f"{S3_URL}/delete-test-bucket")
        assert resp.status_code == 204
        # Verify gone.
        resp = requests.head(f"{S3_URL}/delete-test-bucket")
        assert resp.status_code == 404

    def test_delete_nonexistent_returns_404(self):
        """DELETE /<bucket> for missing bucket returns 404."""
        resp = requests.delete(f"{S3_URL}/nonexistent-bucket-abc")
        assert resp.status_code == 404

    def test_list_buckets_returns_xml(self):
        """GET / returns XML list of all buckets."""
        requests.put(f"{S3_URL}/list-test-a")
        requests.put(f"{S3_URL}/list-test-b")
        resp = requests.get(S3_URL)
        assert resp.status_code == 200
        assert "application/xml" in resp.headers.get("content-type", "")
        assert "<ListAllMyBucketsResult>" in resp.text
        assert "list-test-a" in resp.text
        assert "list-test-b" in resp.text


class TestS3ObjectOperations:
    """S3 object put/get/head/delete operations."""

    @pytest.fixture(autouse=True)
    def _setup(self):
        try:
            wait_for_s3()
        except Exception:
            pytest.skip("S3 gateway not available")
        # Create a test bucket.
        requests.put(f"{S3_URL}/obj-test-bucket")

    def test_put_and_get_object(self):
        """PUT then GET an object."""
        data = b"hello world from e2e test"
        resp = requests.put(
            f"{S3_URL}/obj-test-bucket/test-key.txt",
            data=data,
        )
        assert resp.status_code == 200

        resp = requests.get(f"{S3_URL}/obj-test-bucket/test-key.txt")
        assert resp.status_code == 200
        assert resp.content == data

    def test_head_object(self):
        """HEAD returns 200 for existing object."""
        requests.put(f"{S3_URL}/obj-test-bucket/head-key", data=b"data")
        resp = requests.head(f"{S3_URL}/obj-test-bucket/head-key")
        assert resp.status_code == 200

    def test_delete_object(self):
        """DELETE removes the object."""
        requests.put(f"{S3_URL}/obj-test-bucket/del-key", data=b"data")
        resp = requests.delete(f"{S3_URL}/obj-test-bucket/del-key")
        assert resp.status_code in (200, 204)

    def test_get_nonexistent_object(self):
        """GET on missing key returns 404."""
        resp = requests.get(f"{S3_URL}/obj-test-bucket/no-such-key")
        assert resp.status_code == 404
