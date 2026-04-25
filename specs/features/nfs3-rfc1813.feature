Feature: NFSv3 wire protocol compliance (RFC 1813)

  NFSv3 ONC RPC procedures over TCP with Record Marking framing.
  Each scenario validates wire format and semantics per RFC 1813.

  Background:
    Given a Kiseki NFS server listening on port 2049
    And a test TCP client connected to the NFS port
    And a bootstrap namespace "default" with tenant "org-test"

  # §3.3.0 — NULL
  @unit
  Scenario: RFC1813 §3.3.0 NULL — server responds to ping
    When the client sends an ONC RPC CALL for program 100003 version 3 procedure 0
    Then the server responds with RPC REPLY MSG_ACCEPTED SUCCESS
    And the response body is empty

  @unit
  Scenario: RFC1813 §3.3.0 NULL — wrong program number rejected
    When the client sends an ONC RPC CALL for program 999999 version 3 procedure 0
    Then the server responds with RPC REPLY MSG_ACCEPTED PROG_UNAVAIL

  # §3.3.1 — GETATTR
  @unit
  Scenario: RFC1813 §3.3.1 GETATTR — root directory attributes
    Given the root file handle for namespace "default"
    When the client sends GETATTR with the root file handle
    Then the response status is NFS3_OK
    And the ftype is NF3DIR (2)
    And the mode includes 0755

  @unit
  Scenario: RFC1813 §3.3.1 GETATTR — stale handle returns NFS3ERR_BADHANDLE
    When the client sends GETATTR with an invalid 32-byte handle
    Then the response status is NFS3ERR_BADHANDLE

  # §3.3.3 — LOOKUP
  @unit
  Scenario: RFC1813 §3.3.3 LOOKUP — existing file found
    Given a file "data.h5" was created via NFS CREATE
    When the client sends LOOKUP for "data.h5" in the root directory
    Then the response status is NFS3_OK
    And a valid file handle is returned

  @unit
  Scenario: RFC1813 §3.3.3 LOOKUP — nonexistent file returns NFS3ERR_NOENT
    When the client sends LOOKUP for "nonexistent.txt" in the root directory
    Then the response status is NFS3ERR_NOENT

  # §3.3.6 — READ
  @unit
  Scenario: RFC1813 §3.3.6 READ — read file data
    Given a file "test.bin" was created with content "hello nfs"
    When the client sends READ on "test.bin" at offset 0 count 1024
    Then the response status is NFS3_OK
    And the data equals "hello nfs"
    And eof is true

  @unit
  Scenario: RFC1813 §3.3.6 READ — read with offset
    Given a file "offset.bin" was created with content "abcdefghijklmnop"
    When the client sends READ on "offset.bin" at offset 4 count 4
    Then the response status is NFS3_OK
    And the data equals "efgh"
    And eof is false

  # §3.3.7 — WRITE
  @unit
  Scenario: RFC1813 §3.3.7 WRITE — write data to file
    Given a file handle from a prior CREATE
    When the client sends WRITE with data "written via nfs3" stable FILE_SYNC
    Then the response status is NFS3_OK
    And the count equals 16
    And the committed field is FILE_SYNC (2)

  @unit
  Scenario: RFC1813 §3.3.7 WRITE — write to invalid handle returns NFS3ERR_BADHANDLE
    When the client sends WRITE to an invalid handle with data "bad"
    Then the response status is NFS3ERR_BADHANDLE

  # §3.3.8 — CREATE
  @unit
  Scenario: RFC1813 §3.3.8 CREATE — create new file
    When the client sends CREATE for "newfile.txt" in the root directory
    Then the response status is NFS3_OK
    And a file handle is returned
    And handle_follows is true

  @unit
  Scenario: RFC1813 §3.3.8 CREATE — file registered in directory
    Given a file "created.txt" was created via NFS CREATE
    When the client sends LOOKUP for "created.txt"
    Then the response status is NFS3_OK

  # §3.3.16 — READDIR
  @unit
  Scenario: RFC1813 §3.3.16 READDIR — list directory entries
    Given files "a.txt" and "b.txt" were created via NFS CREATE
    When the client sends READDIR on the root directory
    Then the response status is NFS3_OK
    And the entries include "." and ".."
    And the entries include "a.txt" and "b.txt"
    And eof is true

  # §3.3.12 — REMOVE
  @unit
  Scenario: RFC1813 §3.3.12 REMOVE — delete existing file
    Given a file "deleteme.txt" was created via NFS CREATE
    When the client sends REMOVE for "deleteme.txt" in the root directory
    Then the response status is NFS3_OK
    And LOOKUP for "deleteme.txt" returns NFS3ERR_NOENT

  @unit
  Scenario: RFC1813 §3.3.12 REMOVE — nonexistent file returns NFS3ERR_NOENT
    When the client sends REMOVE for "nosuchfile.txt"
    Then the response status is NFS3ERR_NOENT

  # §3.3.14 — RENAME
  @unit
  Scenario: RFC1813 §3.3.14 RENAME — rename within same directory
    Given a file "old.txt" was created via NFS CREATE
    When the client sends RENAME from "old.txt" to "new.txt"
    Then the response status is NFS3_OK
    And LOOKUP for "new.txt" succeeds
    And LOOKUP for "old.txt" returns NFS3ERR_NOENT

  # §3.3.20 — FSINFO
  @unit
  Scenario: RFC1813 §3.3.20 FSINFO — filesystem capabilities
    When the client sends FSINFO on the root handle
    Then the response status is NFS3_OK
    And maxfilesize is reported
    And rtmax and wtmax are reported (read/write transfer sizes)

  # §3.3.21 — FSSTAT
  @unit
  Scenario: RFC1813 §3.3.21 FSSTAT — filesystem statistics
    When the client sends FSSTAT on the root handle
    Then the response status is NFS3_OK
    And total bytes and free bytes are reported
    And total files and free files are reported
