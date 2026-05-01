@integration @smoke
Feature: Server Harness Smoke Test
  Validates that the BDD harness can start a real kiseki-server
  binary, connect via gRPC and HTTP, and perform a basic roundtrip.
  This feature exists to prove the harness works — it is the
  foundation for migrating all @integration steps from in-memory
  mocks to real server calls.

  Scenario: S3 PUT and GET roundtrip through running server
    Given a running kiseki-server
    When I PUT "hello-kiseki" to S3 key "default/smoke-test"
    Then I can GET S3 key "default/smoke-test" and receive "hello-kiseki"

  Scenario: gRPC health check against running server
    Given a running kiseki-server
    Then the gRPC health endpoint reports the server is ready
