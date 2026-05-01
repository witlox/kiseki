Feature: NFSv4.2 wire protocol compliance (RFC 7862)

  NFSv4.2 COMPOUND operations over TCP with ONC RPC framing.
  Version 4, minor version 2. Session-based stateful protocol.

  Background:
    Given a running kiseki-server

  # Unit scenarios in crate tests (in-process, no TCP):
  #   nfs4_server::tests::exchange_id_returns_ok_with_client_id
  #   nfs4_server::tests::create_session_returns_ok_with_session_id
  #   nfs4_server::tests::sequence_valid_session_returns_ok
  #   nfs4_server::tests::putrootfh_sets_current_filehandle
  #   nfs4_server::tests::getattr_root_returns_dir_type
  #   nfs4_server::tests::write_returns_ok_with_count_and_file_sync
  #   nfs4_server::tests::open_create_returns_ok_with_stateid
  #   ... (20 total in nfs4_server.rs)

  # --- @integration: real TCP to running server ---

  @integration
  Scenario: NFS NULL procedure responds over TCP
    When a client sends NFS NULL RPC to the server
    Then the server replies with RPC ACCEPT_SUCCESS

  @integration
  Scenario: NFSv4 COMPOUND with PUTROOTFH + GETATTR over TCP
    When a client sends a COMPOUND containing PUTROOTFH and GETATTR
    Then the COMPOUND reply contains NFS4_OK for both operations
    And GETATTR returns type directory for the root filehandle

  @integration
  Scenario: NFSv4 sequential write then read — all bytes survive
    Given a running kiseki-server
    When a client writes "AAAA" at offset 0 via NFSv4 WRITE
    And writes "BBBB" at offset 4 via NFSv4 WRITE
    Then reading 8 bytes at offset 0 returns "AAAABBBB"

  @integration
  Scenario: NFSv4 write 10KB in 4KB chunks then read back
    Given a running kiseki-server
    When a client writes a 10KB file via NFSv4 in 4KB sequential chunks
    Then reading the full file returns all 10KB with correct content

  @integration
  Scenario: S3 PUT then NFS READ cross-protocol roundtrip
    Given a 1KB object written via S3 PUT to "default/cross-test"
    When a client reads the object via NFSv4 COMPOUND READ
    Then the NFS READ returns the same bytes as the S3 PUT

  @integration
  Scenario: NFSv4 OPEN+WRITE then S3 GET cross-protocol roundtrip
    Given a file created via NFSv4 OPEN+WRITE with payload "nfs-wrote-this"
    When a client reads the object via S3 GET
    Then the S3 GET returns "nfs-wrote-this"
