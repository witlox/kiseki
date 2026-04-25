Feature: Workflow Advisory & Client Telemetry — bidirectional steering for HPC/AI workflows
  Clients declare a workflow, advance through phases, and send advisory hints to
  help storage steer placement, prefetch, caching, and QoS. Storage emits
  caller-scoped telemetry feedback (backpressure, locality, materialization lag,
  prefetch effectiveness, QoS headroom) on the same channel. Hints are advisory
  only: correctness, ACL, and quota decisions never depend on them. Telemetry
  never leaks cross-tenant information. The advisory subsystem is isolated from
  the data path.

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

  # --- Workflow lifecycle ---

  @unit
  Scenario: Client attempts a profile not in its allow-list
    When the native client calls DeclareWorkflow with profile "batch-etl"
    Then the call is rejected with "profile_not_allowed"
    And no workflow handle is issued
    And an advisory-audit event "declare-workflow: rejected" is written with reason "profile_not_allowed"
    And the workload's data-path operations remain unaffected

  @unit
  Scenario: Workflow ends on TTL expiry
    Given the workflow was declared with ttl_seconds 60
    When 61 seconds elapse without any advisory activity
    Then the workflow is auto-ended with reason "ttl_expired"
    And an advisory-audit event "end-workflow: ttl_expired" is written
    And subsequent hint submissions with the workflow_id return "workflow_unknown"

  # --- Hints are advisory only (I-WA1) ---

  @unit
  Scenario: Hint presence does not change data-path outcome
    Given a composition "checkpoint.pt" under workload "training-run-42"
    When the client writes 256MB to "checkpoint.pt" WITHOUT any hints
    And separately writes 256MB to "checkpoint-b.pt" WITH a full hint bundle (access pattern, priority, affinity, retention)
    Then both writes produce identical durability, encryption, dedup, and visibility outcomes
    And the effective placement for both may differ (hint honoured for the second) but both are valid per placement policy

  @integration
  Scenario: Advisory channel outage does not affect data path
    Given the advisory subsystem on the client's serving node becomes unresponsive
    When the client issues reads and writes for "checkpoint.pt"
    Then all operations complete with normal latency and durability
    And no data-path operation is delayed, blocked, or reordered by the advisory outage
    And the client observes that hint submissions time out or return "advisory_unavailable"

  @unit
  Scenario: Hint rejection returns the operation's own result unchanged
    Given the workload's allowed priority classes are [batch, bulk] only
    When the client submits a hint { priority: interactive } for an in-flight read
    Then the hint is rejected with "priority_not_allowed"
    And the underlying read completes with the same result, latency class, and error behavior it would have without the hint
    And an advisory-audit event "hint-rejected" is written

  # --- Tenant-hierarchy scoping (I-WA3, I-WA7) ---

  @unit
  Scenario: A workflow cannot cross workload boundaries
    Given workload "training-run-42" and workload "inference-svc-9" both under "org-pharma"
    When the client pinned under "training-run-42" calls DeclareWorkflow
    And then submits a hint referencing composition_id owned by "inference-svc-9"
    Then the hint is rejected with "scope_violation"
    And the advisory-audit event includes only "training-run-42"'s identity, not "inference-svc-9"'s
    And no information about "inference-svc-9"'s compositions is leaked in the error

  @unit
  Scenario: Child scope cannot broaden parent ceiling
    Given "clinical-trials" project ceiling is hints_per_sec 300
    When the tenant admin attempts to set "training-run-42" workload budget to hints_per_sec 500
    Then the control-plane update is rejected with "child_exceeds_parent_ceiling"
    And the workload's effective budget remains its last-valid value

  # --- Telemetry scoping (I-WA5, I-WA6) ---

  @unit
  Scenario: Telemetry is computed over the caller's own resources only
    Given the workload owns compositions [A, B] in pool "fast-nvme"
    And a different workload "neighbour-42" owns compositions [C, D] in the same pool
    When the client subscribes to pool-saturation telemetry for "fast-nvme"
    Then the returned backpressure signal reflects the state of the pool as experienced by A and B
    And cluster-wide utilisation exposed (if any) is bucketed with k-anonymity k>=5 over neighbour workloads
    And no field in the telemetry response allows the caller to infer C or D's traffic

  @unit
  Scenario: Telemetry is not an existence oracle
    Given composition "secret-study/results.h5" exists under "org-other" (a different org)
    When the client queries locality telemetry with composition_id pointing at "secret-study/results.h5"
    Then the call returns "not_found" with the same latency distribution and error shape as a genuinely non-existent composition owned by "org-pharma"
    And no timing, size, or code difference distinguishes "forbidden" from "absent"

  @unit
  Scenario: Locality class is coarsely bucketed
    Given the client reads a 1GB composition spanning chunks on local, same-rack, and remote nodes
    When the client requests locality telemetry for that composition
    Then the response uses enum values from {local-node, local-rack, same-pool, remote, degraded}
    And does not reveal node IDs, rack labels, or device serials
    And cannot be used to map neighbour workloads' placements

  # --- Hints inform but never authorise (I-WA9, I-WA14) ---

  @unit
  Scenario: Affinity hint preference honoured within policy
    Given the workload's allowed affinity is pool "fast-nvme"
    When the client submits a hint { affinity: "fast-nvme", colocate_with: "rack-7" }
    Then the placement engine MAY place new chunks in fast-nvme on rack-7
    And MAY override the hint to satisfy EC durability (I-C4) or retention hold (I-C2b)
    And never places chunks in a pool the workload is not authorised for

  @unit
  Scenario: Hint cannot bypass retention hold
    Given composition "patient-scan.dcm" has a retention hold for 7 years
    When the client sends hint { retention: temp } for "patient-scan.dcm"
    Then the hint is rejected with "retention_policy_conflict"
    And the retention hold remains in effect
    And an advisory-audit event "hint-rejected" is written

  @unit
  Scenario: Hint cannot elevate priority beyond policy max
    Given the workload's policy-allowed maximum priority is "batch"
    When the client submits hint { priority: interactive } for a workflow phase
    Then the hint is rejected with "priority_not_allowed"
    And the phase's effective priority remains "batch"

  # --- Prefetch hints (AI training epoch) ---

  @unit
  Scenario: Prefetch hints for shuffled epoch read order
    Given the workflow is in phase "epoch-0" with profile ai-training
    When the client submits a PrefetchHint with 4096 (composition_id, offset, length) tuples scoped to the workload's own compositions totaling 40GB
    Then the advisory subsystem accepts the hint within the workload's declared_prefetch_bytes budget
    And the view subsystem MAY warm the declared ranges opportunistically
    And the client observes improved cache hit rate for the predicted read order
    And prefetch-effectiveness telemetry for this phase reports hit rate in coarse buckets

  @unit
  Scenario: Prefetch hint beyond budget is capped
    Given the workload's declared_prefetch_bytes budget is 64GB
    When the client submits a PrefetchHint totaling 100GB in a single phase
    Then the advisory subsystem accepts 64GB worth and drops the remainder with "prefetch_budget_exceeded"
    And an advisory-audit event is written
    And data-path reads for the unadopted ranges still succeed normally

  @unit
  Scenario: Collective checkpoint announcement
    Given phase "checkpoint" is active with profile hpc-checkpoint
    When the client submits a CollectiveAnnouncement { ranks: 1024, bytes_per_rank: 4GB, deadline: now+120s }
    Then the advisory subsystem MAY pre-warm write-absorb capacity in the target pool
    And the announcement is advisory — the checkpoint succeeds even if no warm-up occurs
    And capacity is never reserved in a way that starves other tenants of their quota

  # --- Backpressure feedback (I-WA5) ---

  @unit
  Scenario: Soft backpressure signals the caller to slow
    Given the pool "fast-nvme" is at 80% of the caller's declared burst budget
    When the client has a telemetry subscription for the current workflow
    Then a backpressure telemetry event with severity "soft" and retry_after_ms hint is delivered
    And the client MAY slow its submission rate
    And data-path operations continue to be accepted

  @unit
  Scenario: Hard backpressure explicitly requests the caller to stop
    Given the pool is at 100% of the caller's hard budget
    When the client has a telemetry subscription
    Then a backpressure telemetry event with severity "hard" is delivered
    And subsequent submissions by this caller MAY be rejected with "quota_exceeded" on the data path (that is existing I-T2 behavior, not a new consequence of the hint system)

  # --- Identity hygiene (I-WA10) ---

  @unit
  Scenario: Another workload cannot use a leaked workflow_id
    Given "training-run-42" has an active workflow with workflow_id "wf-abc..."
    And "training-run-42" inadvertently logs "wf-abc..." to a place visible to "inference-svc-9"
    When a client authenticated as "inference-svc-9" submits a hint carrying workflow_id "wf-abc..."
    Then the hint is rejected with "workflow_not_found_in_scope"
    And no information about the workflow's existence or phase is revealed
    And the rejection latency and error code are indistinguishable from a workflow_id that was never issued

  @unit
  Scenario: New process gets a new client_id
    Given native client process with client_id "cli-7f3a" is running
    When the process restarts
    Then the new process obtains a new client_id "cli-bb01" from a fresh ≥128-bit CSPRNG draw
    And the advisory registrar rejects any attempt to re-register "cli-7f3a"
    And workflows held by "cli-7f3a" expire via TTL (no reattach protocol is defined in this ADR)
    And the new process must call DeclareWorkflow afresh to obtain a new workflow handle

  # --- Audit (I-WA8) ---

  @unit
  Scenario: All advisory decisions are audited on the tenant shard
    When the client performs, within one workflow:
      | step | action                                           |
      | 1    | DeclareWorkflow(profile=ai-training)              |
      | 2    | PhaseAdvance(epoch-0)                             |
      | 3    | Hint(access_pattern=random) accepted              |
      | 4    | Hint(priority=interactive) rejected               |
      | 5    | PrefetchHint 200GB throttled to 64GB              |
      | 6    | SubscribeTelemetry(backpressure)                  |
      | 7    | EndWorkflow                                       |
    Then seven advisory-audit events are written to the tenant audit shard
    And each event carries the (org, project, workload, client_id, workflow_id, phase_id) correlation
    And cluster-admin exports see workflow_id and phase_tag as opaque hashes only (per I-A3, ADR-015)
    And tenant admin exports see the full correlation per I-A2

  # --- Opt-out (I-WA12) ---

  @unit
  Scenario: Tenant admin disables advisory for a workload
    Given "training-run-42" has Workflow Advisory enabled
    When tenant admin disables advisory for "training-run-42"
    Then new DeclareWorkflow calls from clients under "training-run-42" return "ADVISORY_DISABLED"
    And in-flight workflows are gracefully ended with audit
    And the workload's data-path operations proceed with full performance and correctness
    And cluster admin can observe the opt-out in aggregate state but not the reason

  @unit
  Scenario: Cluster admin disables advisory cluster-wide (incident response)
    Given a suspected advisory-subsystem bug
    When cluster admin disables Workflow Advisory cluster-wide
    Then all tenants see "ADVISORY_DISABLED" on DeclareWorkflow
    And no data-path operation is affected
    And the disable action is audited system-wide

  # --- Adversary gate-0 hardening scenarios ---

  @unit
  Scenario: Hint rejection for unauthorized target is indistinguishable from absent target (I-WA6)
    Given composition_id "comp-neighbour" exists under a different workload
    And composition_id "comp-ghost" has never been allocated under any workload
    When the client submits a hint referencing "comp-neighbour"
    And separately submits a hint referencing "comp-ghost"
    Then both calls return the same error code
    And the response payload structures are byte-identical in size
    And the latency distributions over many samples are statistically indistinguishable
    And no timing, size, or code difference lets the caller tell "forbidden" from "absent"

  @unit
  Scenario: Low-k telemetry response has the same shape as populated-k (I-WA5)
    Given pool "fast-nvme" has only the caller's workload and one neighbour workload active (k=2)
    When the caller subscribes to pool-saturation telemetry
    Then the response contains all fields it would in the k>=5 case
    And neighbour-derived fields carry a fixed sentinel value defined by policy
    And the response size, message timing, and field presence are indistinguishable from the populated case

  @unit
  Scenario: mTLS identity is re-validated per operation (I-WA3)
    Given the client has an active bidi advisory stream under cert "tenant-cert-v1"
    When "tenant-cert-v1" is revoked by the Cluster CA
    Then within a bounded detection interval the advisory subsystem tears the stream down
    And subsequent hints on any resumed stream require a currently-valid cert
    And pre-revocation in-flight operations remain accepted up to the revocation point (per I-WA1)

  @unit
  Scenario: Batched audit for high-rate hint throttling (I-WA8)
    Given the workload sustains 200 hints/sec of which 150/sec are throttled
    When measured over a 60-second window
    Then at least one audit event per unique (workflow_id, rejection_reason) tuple is written per second
    And exact accepted-count and throttled-count per workflow per second are preserved in audit
    And the total audit event volume is bounded below the raw 150/sec figure
    And declare/end/phase/policy-violation events are written per-occurrence without batching

  @unit
  Scenario: Concurrent PhaseAdvance is serialized (I-WA13)
    Given two threads in one native-client process hold the same workflow handle at phase_id 5
    When both call PhaseAdvance(6) concurrently
    Then exactly one call returns success and the workflow advances to phase 6
    And the other call returns "phase_not_monotonic"
    And no intermediate state where two phases are active is ever observable

  @unit
  Scenario: Hints in-flight at EndWorkflow follow a clear boundary
    Given the client has 30 hints buffered in the advisory channel toward its active workflow
    When the client calls EndWorkflow
    Then hints that crossed the server-side receive boundary before End are best-effort processed
    And hints submitted after End return "workflow_unknown"
    And EndWorkflow does not block on buffered hint processing
    And an advisory-audit "end-workflow" event is written containing the count of pre-End hints dropped

  @unit
  Scenario: Draining state during opt-out (I-WA12)
    Given "training-run-42" has two active workflows in phases "epoch-3" and "epoch-7"
    When tenant admin transitions advisory for "training-run-42" from enabled to draining
    Then new DeclareWorkflow calls return "ADVISORY_DISABLED"
    And the two active workflows continue to accept hints within their current phases
    And when a workflow advances phase or hits TTL, it is audit-ended
    And when both active workflows have ended, the tenant admin may transition draining to disabled
    And data-path operations for "training-run-42" are unaffected throughout

  @unit
  Scenario: Policy revocation applies prospectively (I-WA18)
    Given the workflow is in phase "compute" with profile ai-training and priority batch
    When tenant admin removes "ai-training" from the allow-list mid-workflow
    Then the current phase continues normally to completion or TTL
    And the next PhaseAdvance is rejected with "profile_revoked"
    And the workflow remains on its current phase
    And data-path operations for this workflow are unaffected

  @unit
  Scenario: Forbidden advisory target fields are rejected (I-WA11)
    When the client submits a hint whose target field contains a shard_id, log_position, chunk_id, dedup_hash, node_id, or device_id
    Then the hint is rejected with "forbidden_target_field" at the schema-validation layer
    And no ownership check or side effect occurs
    And an advisory-audit event is written

  @unit
  Scenario: DeclareWorkflow returns authorized pool handles (I-WA19)
    Given workload "training-run-42" is authorised for pools with tenant-chosen labels ["fast-nvme", "bulk-nvme"]
    When the client calls DeclareWorkflow with profile ai-training
    Then the response carries an opaque 128-bit workflow handle
    And an `available_pools` list containing one descriptor per authorized pool:
      | field        | shape                                         |
      | handle       | opaque 128-bit value, distinct per workflow   |
      | opaque_label | "fast-nvme" / "bulk-nvme" (tenant-chosen)      |
    And subsequent AffinityHint or PrefetchHint submissions MUST reference one of these handles
    And a handle not in this set is rejected with "scope_not_found" (I-WA6)

  @unit
  Scenario: Policy narrowing revokes active telemetry subscriptions (I-WA18)
    Given workflow "wf-abc" holds a telemetry subscription on pool handle "ph-fast"
    When tenant admin narrows policy so the workload is no longer authorised for the pool underlying "ph-fast"
    Then a terminal StreamWarning { kind: SUBSCRIPTION_REVOKED } is emitted to "wf-abc"
    And the subscription is closed within a bounded interval
    And an advisory-audit event "subscription-revoked" is written to the tenant audit shard
    And data-path access to chunks in that pool is independently denied by data-path authorization
    And the workflow's other subscriptions and hints are unaffected

  @unit
  Scenario: Decommissioned pool returns scope-not-found uniformly (I-WA19, I-WA6)
    Given workflow "wf-abc" holds a valid pool handle "ph-fast"
    When the pool underlying "ph-fast" is decommissioned by the cluster admin
    Then subsequent hints referencing "ph-fast" are rejected with "scope_not_found"
    And the rejection shape (code, payload size, latency distribution) is identical to a never-issued handle (I-WA6)
    And the workflow continues; other handles and subscriptions remain valid

  # --- Gate-1 completeness back-fill (gaps found in post-architect audit) ---

  @unit
  Scenario: Own-hotspot telemetry on caller's contended composition
    Given workflow "wf-abc" owns composition "shared-result.h5" that sees sustained concurrent reads from peer workloads in the same workload-id pool (fan-in)
    And the workload is subscribed to the OWN_HOTSPOT telemetry channel
    When contention crosses the soft threshold
    Then an OwnHotspot telemetry event is emitted to "wf-abc" carrying { composition_id: shared-result.h5, contention: moderate|tight }
    And no composition owned by a different workload is named in any own-hotspot event (I-WA5)
    And the contention value is bucketed (no fine-grained counts)

  @unit
  Scenario: Deadline hint accepted and influences scheduling within policy
    Given the workflow has an active phase with priority batch
    When the client submits a DeadlineHint { composition: "checkpoint.pt", deadline: now + 90s }
    Then the advisory subsystem accepts the hint and emits HintAck OUTCOME_ACCEPTED
    And the write path MAY bias scheduling to meet the deadline (best-effort)
    And failure to meet the deadline is NOT an error — the write succeeds whenever the data path completes it (I-WA1)
    And a deadline in the past is rejected with "hint_too_large" treatment (schema validation) or ignored as advisory

  @unit
  Scenario: Phase summary audit event emitted on ring eviction (I-WA13, ADR-021 §9)
    Given the workflow's phase ring has 64 entries (K = default)
    When the client performs a 65th PhaseAdvance
    Then phase entry 1 is evicted from the ring
    And a PhaseSummaryEvent audit entry is emitted to the tenant audit shard with:
      | field                                | shape                  |
      | from_phase_id / to_phase_id         | raw integers           |
      | total_hints_accepted_bucket          | log2 bucket (not exact) |
      | total_hints_rejected_bucket          | log2 bucket            |
      | duration_ms_bucket                   | log2 bucket            |
    And the event is size-padded to a fixed bucket so its wire size does not leak workflow activity (ADR-021 §9)
    And cluster-admin exports see workflow_id and phase_tag hashed (I-WA8, I-A3)

  @unit
  Scenario: Telemetry subscribe emits audit event
    When the client subscribes to channels [BACKPRESSURE, LOCALITY, QOS_HEADROOM]
    Then a TelemetrySubscribedEvent audit entry is emitted with the list of channel enum names
    And unsubscribe via ACTION_REMOVE emits a corresponding TelemetrySubscribedEvent (ACTION_REMOVE variant)
    And these events go to the tenant audit shard (I-WA8)

  @unit
  Scenario: Priority-class revoked mid-workflow produces priority_revoked on next PhaseAdvance (I-WA18)
    Given the workflow's current phase uses priority batch
    And the workload's allowed priorities were [batch, bulk] at DeclareWorkflow
    When tenant admin narrows allowed priorities to [bulk] only
    Then the current phase continues under the snapshotted priority batch (I-WA18)
    And the next PhaseAdvance is rejected with "priority_revoked"
    And the workflow remains on its current phase

  @unit
  Scenario: StreamWarning lifecycle — budget-exceeded, TTL-soon, cert-near-expiry
    Given the workflow is active with a bidi advisory stream open
    When the workload's hints/sec sustained rate exceeds its cap for >5 seconds
    Then the server emits a StreamWarning { kind: BUDGET_EXCEEDED } on the stream (I-WA7, I-WA8)
    When the workflow's TTL is within 60 seconds of expiry
    Then the server emits StreamWarning { kind: WORKFLOW_TTL_SOON }
    When the client's mTLS cert is within its notBefore/notAfter rollover window (about to expire)
    Then the server emits StreamWarning { kind: CERT_NEAR_EXPIRY }
    And each warning is additionally audited as an advisory-state-transition or informational event

  @unit
  Scenario: Server heartbeat keeps AdvisoryStream alive during idleness
    Given the client has an open bidi advisory stream with no hints and no subscriptions
    When 10 seconds of idleness elapse
    Then the server emits StreamWarning { kind: HEARTBEAT } on the stream
    And idle streams receive heartbeats every 10s ± jitter until closed
    And a client missing three consecutive heartbeats treats the stream as dead and reconnects (client-side obligation)

  @unit
  Scenario: gRPC status code is NOT_FOUND for every scope violation (I-WA6, ADR-021 §8)
    When any of the following happen:
      | case                                          |
      | Hint targets a composition owned by another workload |
      | Hint targets a never-existed composition       |
      | Advisory call presents a stolen workflow_ref from a neighbour |
      | Advisory call presents a never-issued workflow_ref |
    Then every response carries gRPC status code NOT_FOUND (5)
    And no response uses PERMISSION_DENIED (7) or UNAUTHENTICATED (16)
    And the application-level AdvisoryError.code is SCOPE_NOT_FOUND for all cases
    And the response size, timing bucket, and message string are identical across all four cases

  # --- Covert-channel hardening (I-WA15) ---

  @unit
  Scenario: Rejection latency does not leak neighbour state
    Given workload A submits hints that would be rejected due to its own policy
    And workload B submits hints that would be rejected due to pool-wide contention caused by neighbour traffic
    When both rejections are measured over many samples
    Then the latency distributions and error payloads are indistinguishable between A's and B's rejections
    And neither A nor B can infer the other's activity from rejection timing

  @unit
  Scenario: Telemetry response size is bucketed
    When a client subscribes to telemetry at different cluster load levels
    Then the size of each telemetry message is one of a small fixed set of sizes (padded/bucketed)
    And the client cannot infer neighbour load from message size variation
