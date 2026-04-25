Feature: Erasure coding — chunk durability across devices (ADR-005, ADR-024)

  Chunks split into data + parity fragments distributed across
  JBOD devices. Survives device failures up to parity count.

  Background:
    Given a pool "fast-nvme" with EC 4+2 on 6 NVMe devices
    And a pool "bulk-hdd" with EC 8+3 on 12 HDD devices

  # === Repair ===

  @integration
  Scenario: Device failure triggers automatic repair (I-D1)
    Given device d3 fails
    When repair is triggered
    Then all chunks with fragments on d3 are identified
    And for each chunk: read remaining fragments, reconstruct d3's fragment
    And write reconstructed fragment to a healthy device
    And update chunk metadata with new placement

  @integration
  Scenario: Repair during normal I/O
    Given a repair is in progress for device d3
    When new writes target pool "fast-nvme"
    Then new writes succeed (placed on healthy devices, skipping d3)
    And repair runs at bounded rate (rebalance_rate_mb_s)

  # === Placement (CRUSH-like, I-D4) ===

  @integration
  Scenario: Device addition triggers rebalance
    Given pool "fast-nvme" has 6 devices
    When device d7 is added
    Then some fragments are migrated to d7 (rebalance)
    And placement is recomputed for affected chunks

