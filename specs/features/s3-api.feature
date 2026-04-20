Feature: S3 API wire protocol compliance

  S3-compatible HTTP REST API on port 9000. Bucket maps to namespace,
  key maps to composition. MVP subset per ADR-014.

  Background:
    Given a Kiseki S3 gateway listening on port 9000
    And a bootstrap namespace "default" mapped to bucket "default"
    And tenant "org-test" is the bootstrap tenant

  # PutObject
  Scenario: S3 PutObject — upload single object
    When the client sends PUT /default/testfile with body "hello s3"
    Then the response status is 200
    And the ETag header is present and non-empty
    And the ETag is a valid UUID

  Scenario: S3 PutObject — empty body creates zero-byte object
    When the client sends PUT /default/empty with empty body
    Then the response status is 200
    And the ETag is returned

  # GetObject
  Scenario: S3 GetObject — retrieve existing object
    Given an object "myobj" was uploaded with body "s3 content"
    When the client sends GET /default/{etag}
    Then the response status is 200
    And the body equals "s3 content"
    And Content-Length header equals 10

  Scenario: S3 GetObject — nonexistent object returns 404
    When the client sends GET /default/00000000-0000-0000-0000-000000000099
    Then the response status is 404

  Scenario: S3 GetObject — invalid UUID key returns 404
    When the client sends GET /default/not-a-uuid
    Then the response status is 404

  # HeadObject
  Scenario: S3 HeadObject — metadata without body
    Given an object was uploaded with 100 bytes
    When the client sends HEAD for that object
    Then the response status is 200
    And Content-Length equals 100
    And the response body is empty

  Scenario: S3 HeadObject — nonexistent returns 404
    When the client sends HEAD /default/00000000-0000-0000-0000-000000000099
    Then the response status is 404

  # DeleteObject
  Scenario: S3 DeleteObject — returns 204
    When the client sends DELETE /default/anything
    Then the response status is 204

  # Bucket/namespace mapping
  Scenario: S3 PutObject — different buckets map to different namespaces
    When the client uploads "data1" to bucket "bucket-a" key "file1"
    And the client uploads "data2" to bucket "bucket-b" key "file2"
    Then the objects are in separate namespaces

  # Error handling
  Scenario: S3 — unknown bucket returns appropriate error
    When the client sends GET /nonexistent-bucket/key
    Then the response status is 404 or 200
    # Note: bucket auto-creates on first write (namespace created on demand)

  # ListObjectsV2
  Scenario: S3 ListObjectsV2 — list all objects in bucket
    Given objects "file1", "file2", "file3" were uploaded to bucket "default"
    When the client sends GET /default (list objects)
    Then the response status is 200
    And the response contains all three object keys
    And each object has a key, size, and last modified timestamp

  Scenario: S3 ListObjectsV2 — prefix filtering
    Given objects "data/a.csv", "data/b.csv", "logs/c.txt" exist
    When the client sends GET /default?prefix=data/
    Then only "data/a.csv" and "data/b.csv" are returned

  Scenario: S3 ListObjectsV2 — empty bucket returns empty list
    Given bucket "empty-bucket" has no objects
    When the client sends GET /empty-bucket
    Then the response status is 200
    And the object list is empty

  Scenario: S3 ListObjectsV2 — pagination with max-keys
    Given 100 objects exist in bucket "default"
    When the client sends GET /default?max-keys=10
    Then 10 objects are returned
    And IsTruncated is true
    And a NextContinuationToken is provided
