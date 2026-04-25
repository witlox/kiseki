Feature: S3 API wire protocol compliance

  S3-compatible HTTP REST API on port 9000. Bucket maps to namespace,
  key maps to composition. MVP subset per ADR-014.

  Background:
    Given a Kiseki S3 gateway listening on port 9000
    And a bootstrap namespace "default" mapped to bucket "default"
    And tenant "org-test" is the bootstrap tenant

  # PutObject
  @unit
  Scenario: S3 PutObject — empty body creates zero-byte object
    When the client sends PUT /default/empty with empty body
    Then the response status is 200
    And the ETag is returned

  # GetObject
  @unit
  Scenario: S3 GetObject — nonexistent object returns 404
    When the client sends GET /default/00000000-0000-0000-0000-000000000099
    Then the response status is 404

  @unit
  Scenario: S3 GetObject — invalid UUID key returns 404
    When the client sends GET /default/not-a-uuid
    Then the response status is 404

  # HeadObject
  @unit
  Scenario: S3 HeadObject — metadata without body
    Given an object was uploaded with 100 bytes
    When the client sends HEAD for that object
    Then the response status is 200
    And Content-Length equals 100
    And the response body is empty

  @unit
  Scenario: S3 HeadObject — nonexistent returns 404
    When the client sends HEAD /default/00000000-0000-0000-0000-000000000099
    Then the response status is 404

  # DeleteObject
  @unit
  Scenario: S3 DeleteObject — returns 204
    When the client sends DELETE /default/anything
    Then the response status is 204

  # ListObjectsV2
  @unit
  Scenario: S3 ListObjectsV2 — prefix filtering
    Given objects "data/a.csv", "data/b.csv", "logs/c.txt" exist
    When the client sends GET /default?prefix=data/
    Then only "data/a.csv" and "data/b.csv" are returned

  @unit
  Scenario: S3 ListObjectsV2 — pagination with max-keys
    Given 100 objects exist in bucket "default"
    When the client sends GET /default?max-keys=10
    Then 10 objects are returned
    And IsTruncated is true
    And a NextContinuationToken is provided
