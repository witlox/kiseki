Feature: Block Storage — raw device I/O, allocation, crash recovery (ADR-029)

  Raw block device management for chunk ciphertext. Auto-detects device
  characteristics. Bitmap allocator with redb journal. File-backed
  fallback for VMs/CI. Per-extent CRC32 for corruption detection.

  Background:
    Given a Kiseki server with data devices configured

  # === Device initialization ===

  @integration
  Scenario: Initialize a raw block device
    Given a raw block device at "/dev/nvme0n1" with 4TB capacity
    When the device is initialized for Kiseki
    Then a superblock is written at offset 0 with magic "KISEKI\x01\x00"
    And a primary allocation bitmap is created after the superblock
    And a mirror allocation bitmap is created after the primary
    And all bitmap bits are cleared (entire data region is free)
    And the device is ready for extent allocation

  @integration
  Scenario: Refuse to initialize device with existing Kiseki superblock
    Given a device already initialized with Kiseki superblock
    When initialization is attempted without --force
    Then the operation is rejected with "device already initialized"
    And no data is overwritten

  @integration
  Scenario: Refuse to initialize device with existing filesystem
    Given a device with XFS filesystem signature
    When initialization is attempted
    Then the operation is rejected with "existing filesystem detected"
    And the error message includes the detected filesystem type

  @integration
  Scenario: Force-initialize over existing superblock
    Given a device already initialized with Kiseki superblock
    When initialization is attempted with --force
    Then the device is re-initialized with a new superblock
    And all previous data is lost
    And the operation is recorded in the audit log

  # === Auto-detection ===

  @integration
  Scenario: Auto-detect NVMe SSD characteristics
    Given a device at "/dev/nvme0n1"
    When device characteristics are probed
    Then medium is detected as "NvmeSsd"
    And physical_block_size is 4096
    And rotational is false
    And io_strategy is "DirectAligned"
    And supports_trim is true

  @integration
  Scenario: Auto-detect HDD characteristics
    Given a device at "/dev/sda" with rotational=1
    When device characteristics are probed
    Then medium is detected as "Hdd"
    And rotational is true
    And io_strategy is "BufferedSequential"

  @integration
  Scenario: Auto-detect virtual device (VM)
    Given a device with "virtio" in model string
    When device characteristics are probed
    Then medium is detected as "Virtual"
    And io_strategy is "FileBacked"

  @integration
  Scenario: File-backed fallback when no block device
    Given a file path "/tmp/kiseki-test-device"
    When a file-backed device is opened
    Then io_strategy is "FileBacked"
    And alignment is enforced at 4096 bytes (simulated)
    And all DeviceBackend operations work identically to raw block

  # === Extent allocation ===

  @unit
  Scenario: Allocate an extent on an empty device
    Given an initialized device with 1GB capacity and 4K block size
    When 256KB is allocated
    Then an extent is returned with offset in the data region
    And the extent length is 256KB (64 blocks at 4K)
    And the corresponding bitmap bits are marked allocated

  @unit
  Scenario: Allocation is block-aligned
    Given a device with physical_block_size 4096
    When 513 bytes is allocated
    Then the extent length is one physical block (4096 bytes)

  @unit
  Scenario: Allocation fails when device is full
    Given a device with 99% of blocks allocated
    When an allocation exceeding remaining free space is attempted
    Then the allocation fails with "device full" error

  @unit
  Scenario: Free an extent and reclaim space
    Given an extent of 256KB was previously allocated
    When the extent is freed
    Then the corresponding bitmap bits are cleared
    And the free-list gains a new free extent
    And device used_bytes decreases by 256KB

  @unit
  Scenario: Adjacent free extents are coalesced
    Given three consecutive 64KB extents were allocated
    When the middle extent is freed
    Then the free-list contains one 64KB free extent
    When the first extent is freed
    Then the two adjacent free extents merge into one 128KB extent
    When the third extent is freed
    Then all three merge into one 192KB extent

  @unit
  Scenario: Large allocation split into multiple extents
    Given maximum extent size is 16MB
    When 32MB is requested
    Then two extents of 16MB each are allocated
    And both are returned as a Vec<Extent>

  # === Data I/O ===

  @integration
  Scenario: Write and read data round-trip
    Given an initialized device
    When 1MB of test data is written to an allocated extent
    And the data is read back from the same extent
    Then the read data matches the written data exactly

  @integration
  Scenario: Write includes CRC32 trailer
    Given an initialized device
    When data is written to an extent
    Then a CRC32 checksum is appended as a 4-byte trailer
    And the total stored size includes the CRC32

  @integration
  Scenario: Read verifies CRC32 on every read
    Given data was written to an extent with CRC32 trailer
    When the extent is read
    Then the CRC32 is verified before returning data
    And the CRC32 trailer is stripped from the returned data

  @integration
  Scenario: CRC32 mismatch detected as corruption
    Given data was written to an extent
    When a bit flip is simulated in the stored data
    And the extent is read
    Then the CRC32 verification fails
    And a "data corruption" error is returned (not "authentication failure")

  @integration
  Scenario: O_DIRECT write on NVMe (DirectAligned strategy)
    Given a device with io_strategy "DirectAligned"
    When data is written
    Then the write uses O_DIRECT flag (bypasses page cache)
    And the write buffer is aligned to physical_block_size

  @integration
  Scenario: Buffered write on HDD (BufferedSequential strategy)
    Given a device with io_strategy "BufferedSequential"
    When data is written
    Then the write does NOT use O_DIRECT
    And fdatasync is called after write

  # === Crash recovery ===

  @integration
  Scenario: Crash between journal and bitmap — recovered on restart
    Given an allocation was journaled in redb
    But the bitmap was NOT updated (simulated crash)
    When the device is reopened
    Then the journal entry is replayed
    And the bitmap is updated to match the journal
    And the free-list is rebuilt from the corrected bitmap

  @integration
  Scenario: Crash between data write and chunk_meta — extent reclaimed
    Given an extent was allocated and data was written
    But chunk_meta was NOT committed to redb (simulated crash)
    When the device is reopened and scrub runs
    Then the orphan extent is detected (bitmap set, no chunk_meta)
    And the extent is freed (bitmap cleared, journal entry removed)
    And no data loss occurs (the write was never acknowledged)

  @integration
  Scenario: Bitmap primary/mirror mismatch — resolved from journal
    Given the primary bitmap was updated but the mirror was not (crash)
    When the device is reopened
    Then the mismatch is detected
    And the bitmap consistent with the redb journal is used
    And the other copy is repaired to match

  @integration
  Scenario: Superblock corruption detected on open
    Given the superblock checksum does not match its contents
    When the device is opened
    Then the device is marked as corrupted
    And no allocations or I/O are permitted
    And an alert is raised to the cluster admin

  # === Periodic scrub ===

  @integration
  Scenario: Scrub detects orphan extents
    Given 3 extents are allocated in bitmap but have no chunk_meta in redb
    When periodic scrub runs
    Then all 3 orphan extents are freed
    And bitmap bits are cleared
    And scrub reports "3 orphan extents reclaimed, 768KB freed"

  @integration
  Scenario: Scrub detects bitmap inconsistency
    Given bitmap shows block 1000 as free but redb has a chunk_meta pointing to it
    When scrub runs
    Then the bitmap is corrected (block 1000 marked allocated)
    And scrub reports "1 bitmap inconsistency corrected"

  @integration
  Scenario: Scrub runs on device startup and periodically
    When a device is opened
    Then an initial scrub runs during startup
    And subsequent scrubs run every 6 hours by default

  # === TRIM batching ===

  @integration
  Scenario: TRIM commands are batched, not immediate
    Given a device with supports_trim = true
    When 100 small extents are freed in rapid succession
    Then no TRIM commands are issued immediately
    And the freed extents accumulate in a TRIM queue
    When the TRIM flush interval fires (60 seconds)
    Then a single batched BLKDISCARD covers all 100 extents

  # === Capacity reporting ===

  @integration
  Scenario: Device reports accurate capacity
    Given an initialized 1TB device with 100GB allocated
    When capacity is queried
    Then used_bytes is 100GB minus superblock and bitmap overhead
    And total_bytes is 1TB
    And the values account for superblock and bitmap overhead

  # === Additional crash recovery and validation scenarios ===

  @integration
  Scenario: WAL intent entry detected on restart — extent freed if no chunk_meta
    Given an extent was allocated with a WAL intent entry
    But no chunk_meta was committed for that extent
    When the device is reopened
    Then the WAL intent entry is detected during startup scrub
    And the extent is freed (bitmap cleared)
    And the WAL intent entry is removed

  @integration
  Scenario: Superblock checksum verified on every open
    Given an initialized device
    When the device is opened
    Then the superblock checksum is verified against its contents
    And any mismatch prevents the device from being used

  @integration
  Scenario: Free-list rebuilt from bitmap on restart
    Given a device with 50 extents allocated
    When the device is reopened
    Then the free-list is rebuilt from the bitmap
    And allocations work correctly after rebuild

  @integration
  Scenario: Unknown superblock version rejected
    Given a device with superblock version 99
    When the device is opened
    Then the open fails with "unsupported version" error
