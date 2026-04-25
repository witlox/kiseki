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

  # §3.3.3 — LOOKUP
  @unit
  Scenario: RFC1813 §3.3.3 LOOKUP — nonexistent file returns NFS3ERR_NOENT
    When the client sends LOOKUP for "nonexistent.txt" in the root directory
    Then the response status is NFS3ERR_NOENT

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

  # §3.3.12 — REMOVE
  @unit
  Scenario: RFC1813 §3.3.12 REMOVE — nonexistent file returns NFS3ERR_NOENT
    When the client sends REMOVE for "nosuchfile.txt"
    Then the response status is NFS3ERR_NOENT

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
