Feature: Composition — Tenant-scoped data assembly and namespace management
  The Composition context maintains metadata structures describing how
  chunks assemble into data units (files, objects). It mediates all
  writes: translating protocol-level operations into deltas for the Log
  and chunk writes for Chunk Storage. Manages namespaces and refcounting.

  Background:
    Given a Kiseki cluster with tenant "org-pharma"
    And namespace "trials" in shard "shard-trials-1"
    And tenant KEK "pharma-kek-001" is active

  # --- Namespace management ---

  @library
  Scenario: Create namespace
    Given tenant admin for "org-pharma" requests new namespace "genomics"
    When the Control Plane approves (quota, policy check)
    Then a new shard is created for "genomics"
    And the namespace is associated with the tenant and shard
    And compliance tags from the org level are inherited
