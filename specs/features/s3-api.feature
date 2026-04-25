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
