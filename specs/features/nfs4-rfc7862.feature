Feature: NFSv4.2 wire protocol compliance (RFC 7862)

  NFSv4.2 COMPOUND operations over TCP with ONC RPC framing.
  Version 4, minor version 2. Session-based stateful protocol.

  Background:
    Given a Kiseki NFS server listening on port 2049
    And a test TCP client connected to the NFS port
    And a bootstrap namespace "default" with tenant "org-test"

  # §18.35 — EXCHANGE_ID
  Scenario: RFC7862 §18.35 EXCHANGE_ID — client registration
    When the client sends COMPOUND with EXCHANGE_ID
    Then the response status is NFS4_OK
    And a client_id is returned (non-zero u64)
    And server_owner contains a valid major_id
    And the flags include CONFIRMED

  Scenario: RFC7862 §18.35 EXCHANGE_ID — returns unique client IDs
    When two clients send EXCHANGE_ID
    Then the returned client_ids are different

  # §18.36 — CREATE_SESSION
  Scenario: RFC7862 §18.36 CREATE_SESSION — session established
    Given a client_id from EXCHANGE_ID
    When the client sends COMPOUND with CREATE_SESSION for that client_id
    Then the response status is NFS4_OK
    And a 16-byte session_id is returned
    And fore_channel_attrs include maxops and maxreqs

  Scenario: RFC7862 §18.36 CREATE_SESSION — random session IDs
    Given two sessions are created
    Then the session_ids are cryptographically distinct

  # §18.46 — SEQUENCE
  Scenario: RFC7862 §18.46 SEQUENCE — valid session accepted
    Given an active session
    When the client sends COMPOUND with SEQUENCE using that session_id
    Then the response status is NFS4_OK
    And the returned sequenceid and slotid are valid

  Scenario: RFC7862 §18.46 SEQUENCE — invalid session returns NFS4ERR_BADSESSION
    When the client sends SEQUENCE with a fabricated session_id
    Then the response status is NFS4ERR_BADSESSION

  # §18.24 — PUTROOTFH
  Scenario: RFC7862 §18.24 PUTROOTFH — sets current filehandle to root
    Given an active session
    When the client sends COMPOUND with SEQUENCE + PUTROOTFH + GETFH
    Then PUTROOTFH status is NFS4_OK
    And GETFH returns a valid root file handle

  # §18.9 — GETATTR
  Scenario: RFC7862 §18.9 GETATTR — root attributes
    Given the current filehandle is the root
    When the client sends GETATTR with bitmap requesting type and size
    Then the response status is NFS4_OK
    And the type is NF4DIR
    And the size is returned

  Scenario: RFC7862 §18.9 GETATTR — no filehandle returns NFS4ERR_BADHANDLE
    When the client sends GETATTR without setting a filehandle first
    Then the response status is NFS4ERR_BADHANDLE

  # §18.25 — READ
  Scenario: RFC7862 §18.25 READ — read file via COMPOUND
    Given a file was created via COMPOUND WRITE
    When the client sends COMPOUND with SEQUENCE + READ at offset 0
    Then the response status is NFS4_OK
    And the data matches what was written

  Scenario: RFC7862 §18.25 READ — read past EOF returns empty with eof=true
    Given a small file exists
    When the client sends READ at offset beyond file size
    Then the response status is NFS4_OK
    And eof is true
    And data is empty

  # §18.38 — WRITE
  Scenario: RFC7862 §18.38 WRITE — write data via COMPOUND
    Given the current filehandle is a writable file
    When the client sends COMPOUND with SEQUENCE + WRITE with data "nfs4 write"
    Then the response status is NFS4_OK
    And count equals 10
    And committed is FILE_SYNC

  Scenario: RFC7862 §18.38 WRITE — write updates current filehandle
    When the client sends COMPOUND with WRITE + GETFH
    Then GETFH returns the handle of the newly written file

  # §18.37 — DESTROY_SESSION
  Scenario: RFC7862 §18.37 DESTROY_SESSION — session teardown
    Given an active session
    When the client sends DESTROY_SESSION with that session_id
    Then the response status is NFS4_OK
    And subsequent SEQUENCE with that session_id returns NFS4ERR_BADSESSION

  Scenario: RFC7862 §18.37 DESTROY_SESSION — unknown session
    When the client sends DESTROY_SESSION with a nonexistent session_id
    Then the response status is NFS4ERR_BADSESSION

  # §15.5 — IO_ADVISE (NFSv4.2 specific)
  Scenario: RFC7862 §15.5 IO_ADVISE — hint accepted
    Given an active session and a file handle
    When the client sends IO_ADVISE with sequential read hint
    Then the response status is NFS4_OK

  Scenario: RFC7862 §15.5 IO_ADVISE — hints are advisory only
    Given an active session
    When the client sends IO_ADVISE with an unsupported hint
    Then the response status is NFS4_OK
    And the hints bitmap may be empty (server accepted but ignored)

  # COMPOUND limits
  Scenario: RFC7862 — COMPOUND ops capped at 32
    When the client sends COMPOUND with 100 operations
    Then only the first 32 are processed
    And the response contains at most 32 op results
