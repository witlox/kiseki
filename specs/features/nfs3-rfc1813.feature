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
