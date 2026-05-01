Feature: Workflow Advisory & Client Telemetry — bidirectional steering for HPC/AI workflows
  Clients declare a workflow, advance through phases, and send advisory hints to
  help storage steer placement, prefetch, caching, and QoS. Storage emits
  caller-scoped telemetry feedback (backpressure, locality, materialization lag,
  prefetch effectiveness, QoS headroom) on the same channel. Hints are advisory
  only: correctness, ACL, and quota decisions never depend on them. Telemetry
  never leaks cross-tenant information. The advisory subsystem is isolated from
  the data path.

  # 41 @unit scenarios moved to crate-level unit tests:
  # - kiseki-advisory/tests/workflow_advisory_unit.rs (48 tests)
  # - kiseki-advisory/src/policy.rs, telemetry.rs, workflow.rs, stream.rs, lookup.rs

  Background:
    Given a Kiseki cluster with Workflow Advisory enabled cluster-wide
    And organization "org-pharma" with project "clinical-trials" and workload "training-run-42"
    And a native client process pinned as client_id "cli-7f3a" under "training-run-42"
    And the workload's hint budget is:
      | field                      | value |
      | hints_per_sec              | 200   |
      | concurrent_workflows       | 4     |
      | phases_per_workflow        | 64    |
      | telemetry_subscribers      | 4     |
      | declared_prefetch_bytes    | 64GB  |
    And the workload's allowed profiles are [ai-training, ai-inference, hpc-checkpoint]
    And workflow_declares_per_sec is 10 and max_prefetch_tuples_per_hint is 4096

  @library
  Scenario: Advisory channel outage does not affect data path
    Given the advisory subsystem on the client's serving node becomes unresponsive
    When the client issues reads and writes for "checkpoint.pt"
    Then all operations complete with normal latency and durability
    And no data-path operation is delayed, blocked, or reordered by the advisory outage
    And the client observes that hint submissions time out or return "advisory_unavailable"
