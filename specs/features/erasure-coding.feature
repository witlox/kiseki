Feature: Erasure coding — chunk durability across devices (ADR-005, ADR-024)

  Chunks split into data + parity fragments distributed across
  JBOD devices. Survives device failures up to parity count.

  Background:
    Given a pool "fast-nvme" with EC 4+2 on 6 NVMe devices
    And a pool "bulk-hdd" with EC 8+3 on 12 HDD devices

  # === Write path ===

  Scenario: Chunk written with EC 4+2
    When a 1MB chunk is written to pool "fast-nvme"
    Then the chunk is split into 4 data fragments (256KB each)
    And 2 parity fragments are computed
    And all 6 fragments are written to distinct devices (I-D4)

  Scenario: Chunk written with EC 8+3
    When a 4MB chunk is written to pool "bulk-hdd"
    Then 8 data fragments + 3 parity fragments are created
    And all 11 fragments are on distinct devices

  Scenario: Small chunk below EC minimum
    When a 4KB chunk is written to pool "fast-nvme"
    Then EC is still applied (4 × 1KB data + 2 × 1KB parity)
    And 6 fragments are stored

  # === Read path ===

  Scenario: Normal read — all fragments available
    Given a chunk with EC 4+2 on devices [d1..d6]
    When the chunk is read
    Then only the 4 data fragments are read (d1..d4)
    And parity fragments are not read (fast path)
    And the chunk is reassembled from data fragments

  Scenario: Degraded read — one device unavailable
    Given a chunk with EC 4+2 and device d3 is offline
    When the chunk is read
    Then 3 data fragments (d1, d2, d4) + 1 parity fragment (d5) are read
    And the missing fragment is reconstructed via EC math
    And the chunk is returned successfully

  Scenario: Degraded read — two devices unavailable (max for 4+2)
    Given devices d3 and d5 are offline
    When the chunk is read
    Then 2 data (d1, d2) + 2 remaining (d4, d6) are read
    And 2 missing fragments reconstructed from parity
    And the chunk is returned

  Scenario: Read fails — too many devices offline
    Given devices d3, d5, and d6 are offline (3 > parity count 2)
    When the chunk is read
    Then reconstruction fails
    And a ChunkLost error is returned

  # === Repair ===

  Scenario: Device failure triggers automatic repair (I-D1)
    Given device d3 fails
    When repair is triggered
    Then all chunks with fragments on d3 are identified
    And for each chunk: read remaining fragments, reconstruct d3's fragment
    And write reconstructed fragment to a healthy device
    And update chunk metadata with new placement

  Scenario: Repair during normal I/O
    Given a repair is in progress for device d3
    When new writes target pool "fast-nvme"
    Then new writes succeed (placed on healthy devices, skipping d3)
    And repair runs at bounded rate (rebalance_rate_mb_s)

  # === Placement (CRUSH-like, I-D4) ===

  Scenario: Fragments deterministically placed
    Given the same chunk_id and pool device list
    When placement is computed twice
    Then the same devices are selected both times (deterministic)

  Scenario: Device addition triggers rebalance
    Given pool "fast-nvme" has 6 devices
    When device d7 is added
    Then some fragments are migrated to d7 (rebalance)
    And placement is recomputed for affected chunks

  # === Storage efficiency ===

  Scenario: EC 4+2 storage overhead is 1.5x
    When 100GB of data is written to pool "fast-nvme" (EC 4+2)
    Then 150GB of storage is used (100GB data + 50GB parity)
    And storage efficiency is 67%

  Scenario: EC 8+3 storage overhead is 1.375x
    When 100GB of data is written to pool "bulk-hdd" (EC 8+3)
    Then 137.5GB of storage is used
    And storage efficiency is 73%

  # === Replication mode (meta pool) ===

  Scenario: Meta pool uses replication-3 (not EC)
    Given pool "meta-nvme" with replication-3
    When a chunk is written
    Then 3 identical copies are stored on 3 distinct devices
    And any single copy can serve reads (no reconstruction needed)
