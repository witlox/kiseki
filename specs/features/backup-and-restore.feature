Feature: External backup and restore (ADR-016)

  Cluster admin can configure an external backup target. Backups
  contain encrypted shard metadata and (optionally) chunk data. The
  same `BackupManager` works against either a local-filesystem
  backend OR an S3-compatible object store — the trait is the seam.

  ADR-016 §"External backup": backup is additive to federation, not
  the primary DR mechanism. Snapshot-only restore (no point-in-time
  replay).

  Background:
    Given a backup-capable cluster

  # === Filesystem backend ===

  @integration
  Scenario: Snapshot persisted on filesystem backend
    Given a filesystem backup backend is configured
    And shard "s1" with 1024 bytes of metadata
    And shard "s2" with 2048 bytes of metadata
    When the operator triggers a backup
    Then a snapshot tarball lands in the backup directory
    And the snapshot manifest records 2 shards

  @integration
  Scenario: Snapshot round-trips back to live shards
    Given a filesystem backup backend is configured
    And shard "alpha" with 512 bytes of metadata and chunk data
    When the operator triggers a backup
    And the operator restores the most recent snapshot
    Then 1 shard is recovered with metadata and chunk data intact

  # === S3 backend ===

  @integration
  Scenario: Snapshot persisted on S3 backend through SigV4
    Given an S3-compatible backup backend is configured
    And shard "s1" with 1024 bytes of metadata
    When the operator triggers a backup
    Then the snapshot tarball is reachable through the S3 backend
    And the manifest is reachable through the S3 backend

  @integration
  Scenario: List surfaces every snapshot regardless of backend
    Given a filesystem backup backend is configured
    When the operator triggers a backup
    And the operator triggers a backup
    Then listing snapshots returns 2 entries

  # === Operational guarantees ===

  @integration
  Scenario: Concurrent backups are rejected
    Given a filesystem backup backend is configured
    And a backup is already in progress
    When the operator triggers a backup
    Then the second backup is rejected with InProgress
    And the in-progress flag can be cleared and a new backup succeeds

  @integration
  Scenario: Retention cleanup removes expired snapshots
    Given a filesystem backup backend is configured
    And the operator created 3 snapshots
    When retention is enforced with a 0-day window
    Then all 3 snapshots are deleted
