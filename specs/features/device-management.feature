Feature: Device management and pool capacity (ADR-024)

  Device lifecycle, capacity thresholds, automatic evacuation,
  and EC repair. Storage admin manages devices via API.

  Background:
    Given a Kiseki cluster with 2 affinity pools:
      | pool      | device_class | durability | devices |
      | fast-nvme | NVMe-U.2     | EC 4+2     | 6       |
      | bulk-hdd  | HDD-Bulk     | EC 8+3     | 12      |
    And a cluster admin authenticated with admin certificate

  # === Device lifecycle ===

  @integration
  Scenario: Add device to pool
    When the admin adds device "/dev/nvme2n1" to pool "fast-nvme"
    Then the device appears in the pool device list
    And the pool capacity increases by the device size
    And the device state is "Healthy"

  @integration
  Scenario: Evacuate device — chunks migrate to other pool members
    Given device "dev-1" in pool "fast-nvme" has 100 chunks
    When the admin initiates evacuation of "dev-1"
    Then the device state transitions to "Evacuating"
    And chunks are migrated to other healthy devices in "fast-nvme"
    And when migration completes, the device state is "Removed"
    And all 100 chunks remain accessible

  @integration
  Scenario: Cancel evacuation
    Given device "dev-1" is in state "Evacuating" at 30% progress
    When the admin cancels the evacuation
    Then the device state returns to "Degraded"
    And partially migrated chunks are consistent (no duplicates)

  @integration
  Scenario: Device failure triggers automatic EC repair
    Given chunk "c1" has EC 4+2 fragments on devices [d1, d2, d3, d4, d5, d6]
    When device "d3" fails (unresponsive)
    Then the device state transitions to "Failed"
    And EC repair is triggered automatically for all chunks on "d3"
    And chunk "c1" is reconstructed from fragments on [d1, d2, d4, d5, d6]
    And the repaired fragment is placed on a healthy device
    And the repair event is recorded in the audit log

  @unit
  Scenario: Remove device without prior evacuation — blocked
    Given device "dev-1" has chunks stored on it
    When the admin attempts to remove "dev-1" without evacuating
    Then the operation is rejected with "device has data, evacuate first"

  # === Capacity thresholds (per device class) ===

  @integration
  Scenario: NVMe pool enters Warning state at 75%
    Given pool "fast-nvme" is at 74% capacity
    When a write brings it to 76%
    Then the pool health transitions to "Warning"
    And a telemetry event is emitted
    And writes continue to succeed

  @integration
  Scenario: NVMe pool enters Critical state at 85% — new placements rejected
    Given pool "fast-nvme" is at 84% capacity
    When a write brings it to 86%
    Then the pool health transitions to "Critical"
    And new chunk placements to "fast-nvme" are rejected
    And the placement engine redirects to a sibling NVMe pool if available

  @integration
  Scenario: HDD pool tolerates higher fill — Warning at 85%
    Given pool "bulk-hdd" is at 84% capacity
    Then the pool health is still "Healthy"
    When a write brings it to 86%
    Then the pool health transitions to "Warning"

  @integration
  Scenario: Pool at Full returns ENOSPC
    Given pool "fast-nvme" is at 97% (Full for NVMe)
    When a client attempts to write a chunk
    Then the write is rejected with ENOSPC

  @unit
  Scenario: Pool redirection stays within same device class
    Given pool "fast-nvme-a" is Critical
    And pool "fast-nvme-b" is Healthy
    When a chunk targets "fast-nvme-a"
    Then the placement engine redirects to "fast-nvme-b"
    And the chunk is never placed on a HDD pool

  @unit
  Scenario: No sibling pool available — ENOSPC
    Given pool "fast-nvme" is the only NVMe pool and is Critical
    When a chunk targets "fast-nvme"
    Then the write returns ENOSPC (no same-class sibling)

  # === Automatic evacuation on health warnings ===

  @integration
  Scenario: SSD SMART wear triggers auto-evacuation
    Given device "dev-1" in pool "fast-nvme" reports SMART wear 92%
    Then the device is automatically marked "Evacuating"
    And background migration begins without admin intervention
    And an alert is emitted for the cluster admin

  @integration
  Scenario: HDD bad sectors trigger auto-evacuation
    Given device "dev-5" in pool "bulk-hdd" reports 150 reallocated sectors
    Then the device is automatically marked "Evacuating"
    And an alert is emitted

  @unit
  Scenario: Temperature throttling
    Given device "dev-1" reports temperature 82°C
    Then I/O to the device is throttled
    And a warning is logged
    And the device is NOT evacuated (temperature may be transient)

  # === Device state audit trail (I-D2) ===

  @unit
  Scenario: All device state transitions are audited
    When device "dev-1" transitions from "Healthy" to "Degraded"
    Then the audit log contains an entry with:
      | field       | value        |
      | device_id   | dev-1        |
      | old_state   | Healthy      |
      | new_state   | Degraded     |
      | reason      | SMART wear   |
      | admin_id    | (system)     |

  # === EC fragment placement (I-D4) ===

  @unit
  Scenario: EC fragments placed on distinct devices
    When a chunk is written to pool "fast-nvme" with EC 4+2
    Then 6 fragments are created
    And each fragment is on a different device
    And no two fragments share the same device

  @unit
  Scenario: Insufficient devices for EC — write rejected
    Given pool "fast-nvme" has only 3 healthy devices
    And EC requires 4+2 = 6 devices
    When a chunk write is attempted
    Then the write is rejected (insufficient devices for durability)

  # === System partition monitoring ===

  @integration
  Scenario: System partition RAID-1 degraded — warning
    Given the system partition is RAID-1 on 2 SSDs
    When one SSD fails
    Then Kiseki logs a WARNING about degraded system RAID
    And Kiseki continues operating normally
    And the cluster admin is alerted to replace the drive

  @integration
  Scenario: System partition both drives failed — refuse to start
    Given both system RAID-1 drives have failed
    When Kiseki attempts to start
    Then startup is aborted with CRITICAL error
    And the message indicates system partition is unavailable
