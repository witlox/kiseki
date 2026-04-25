Feature: NFSv4.2 wire protocol compliance (RFC 7862)

  NFSv4.2 COMPOUND operations over TCP with ONC RPC framing.
  Version 4, minor version 2. Session-based stateful protocol.

  Background:
    Given a Kiseki NFS server listening on port 2049
    And a test TCP client connected to the NFS port
    And a bootstrap namespace "default" with tenant "org-test"

  # Unit scenarios moved to crate tests:
  #   nfs4_server::tests::exchange_id_returns_ok_with_client_id
  #   nfs4_server::tests::exchange_id_returns_unique_client_ids
  #   nfs4_server::tests::create_session_returns_ok_with_session_id
  #   nfs4_server::tests::create_session_produces_distinct_ids
  #   nfs4_server::tests::sequence_valid_session_returns_ok
  #   nfs4_server::tests::sequence_invalid_session_returns_badsession
  #   nfs4_server::tests::putrootfh_sets_current_filehandle
  #   nfs4_server::tests::getattr_root_returns_dir_type
  #   nfs4_server::tests::getattr_no_filehandle_returns_badhandle
  #   nfs4_server::tests::write_returns_ok_with_count_and_file_sync
  #   nfs4_server::tests::write_updates_current_filehandle
  #   nfs4_server::tests::open_create_returns_ok_with_stateid
  #   nfs4_server::tests::open_read_existing_returns_ok_with_stateid
  #   nfs4_server::tests::open_nonexistent_nocreate_returns_noent
  #   nfs4_server::tests::close_valid_stateid_returns_ok
  #   nfs4_server::tests::close_then_read_returns_bad_stateid
