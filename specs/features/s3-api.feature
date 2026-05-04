Feature: S3 API wire protocol compliance

  S3-compatible HTTP REST API on port 9000. Bucket maps to namespace,
  key maps to composition. MVP subset per ADR-014.

  Background:
    Given a Kiseki S3 gateway listening on port 9000
    And a bootstrap namespace "default" mapped to bucket "default"
    And tenant "org-test" is the bootstrap tenant

  # Unit scenarios moved to crate tests:
  #   s3_server::tests::put_empty_body_returns_200_with_etag
  #   s3_server::tests::get_nonexistent_object_returns_404
  #   s3_server::tests::get_invalid_uuid_returns_404
  #   s3_server::tests::head_object_returns_content_length_and_empty_body
  #   s3_server::tests::head_nonexistent_object_returns_404
  #   s3_server::tests::delete_object_returns_204
  #   s3_server::tests::list_objects_prefix_filtering
  #   s3_server::tests::list_objects_pagination

  # GCP transport-profile run 2026-05-04 (3rd attempt): the bench
  # ran `PUT /<bucket>/<key>` against a bucket name that hadn't
  # been registered via `PUT /<bucket>` first. The gateway
  # returned 500 with body "upstream error: namespace not found:
  # NamespaceId(...)" — operationally opaque, looks like a server
  # failure when it's actually operator error. The contract is now
  # explicit: the S3 layer maps unregistered-bucket writes to the
  # standard S3 404 NoSuchBucket. Sample observability: the
  # gateway's tracing::warn still records the namespace UUID for
  # debugging.
  #
  # Unit scenario covering the contract:
  #   s3_server::tests::put_to_unregistered_bucket_returns_404_no_such_bucket
