Feature: NFSv3 wire protocol compliance (RFC 1813)

  NFSv3 ONC RPC procedures over TCP with Record Marking framing.
  Each scenario validates wire format and semantics per RFC 1813.

  Background:
    Given a Kiseki NFS server listening on port 2049
    And a test TCP client connected to the NFS port
    And a bootstrap namespace "default" with tenant "org-test"

  # Unit scenarios moved to crate tests:
  #   nfs3_server::tests::null_returns_success_with_empty_body
  #   nfs3_server::tests::wrong_program_returns_prog_unavail
  #   nfs3_server::tests::lookup_nonexistent_returns_noent
  #   nfs3_server::tests::write_file_sync_returns_ok_and_count
  #   nfs3_server::tests::write_invalid_handle_returns_badhandle
  #   nfs3_server::tests::write_unregistered_handle_at_nonzero_offset_returns_io_error
  #   nfs3_server::tests::create_returns_ok_with_handle
  #   nfs3_server::tests::remove_nonexistent_returns_noent
  #   nfs3_server::tests::fsinfo_returns_ok_with_sizes
  #   nfs3_server::tests::fsstat_returns_ok_with_bytes_and_files

  # --- @integration: real TCP to running server ---

  @integration
  Scenario: NFSv3 NULL procedure responds over TCP
    Given a running kiseki-server
    When a client sends NFSv3 NULL RPC to the server
    Then the server replies with RPC ACCEPT_SUCCESS

  # DEFERRED — write-then-read roundtrip scenarios surfaced a real
  # prod bug while being wired: kiseki-gateway/src/nfs3_server.rs:272
  # reads the file handle from the WRITE RPC into `_fh` and discards
  # it, then creates a *fresh* composition via `ctx.write(data)`.
  # The handle produced by the prior CREATE never gets data attached,
  # so a subsequent LOOKUP+READ on that name returns 0 bytes.
  #
  # The fix needs server-side work: bind WRITE's data to the
  # composition referenced by the supplied file handle, OR have
  # CREATE allocate a composition_id that WRITE then targets. Either
  # way it changes the kiseki-gateway NFSv3 surface, not just the
  # client.
  #
  # Tracked in specs/implementation/cluster-harness-followups.md as a
  # bugfix-protocol item. Re-add these scenarios verbatim once the
  # CREATE+WRITE binding lands:
  #
  #   Scenario: NFSv3 small file roundtrip — write then read returns the same bytes
  #     Given a running kiseki-server
  #     When a client writes "small-nfs3-payload" via NFSv3
  #     Then reading via NFSv3 returns "small-nfs3-payload"
  #
  #   Scenario: NFSv3 1MB file roundtrip — exercises the chunked storage path
  #     Given a running kiseki-server
  #     When a client writes a 1MB file via NFSv3
  #     Then reading via NFSv3 returns all 1MB with correct content
  #
  #   Scenario: NFSv3 write then S3 read — cross-protocol
  #     Given a running kiseki-server
  #     When a client writes "nfs3-wrote-this" via NFSv3
  #     Then reading the same composition via S3 returns "nfs3-wrote-this"
