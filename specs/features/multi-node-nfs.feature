Feature: NFSv4 protocol behavior on multi-node clusters

  # NFSv4 wire path against a real multi-node `kiseki-server` cluster.
  # Single-node `nfs4-rfc7862.feature` covers protocol compliance
  # against one server; this file covers failure modes that only
  # emerge when the gateway routes through the cross-node fabric
  # (Phase 16+).
  #
  # Regression witness for the GCP 2026-05-02 perf-cluster failure: a
  # 6-node deployment returned EIO on every NFSv4 file CREATE,
  # including `touch`, while S3 PUT on the same gateway succeeded.
  # The single-node NFS scenarios in nfs4-rfc7862.feature did NOT
  # catch this — the 6-node EC 4+2 fabric path is reached only when
  # `defaults_for(>=6)` selects EC. If the empty-file scenario fails,
  # the gateway's write path is broken for empty payloads on the
  # multi-node EC code path.
  @integration @multi-node @nfs
  Scenario: 6-node cluster open create empty file via NFSv4 succeeds
    Given a 6-node kiseki cluster
    When a client opens-and-creates an empty file via NFSv4 on node-1
    Then the NFSv4 COMPOUND status is NFS4_OK
    And a composition id is returned in the GETFH reply

  # Companion to the empty-file scenario: a non-empty CREATE+WRITE
  # COMPOUND. Exercises the chunk path (bytes > 0 → fabric replicate
  # / EC encode) where the empty scenario exercises the 0-byte path.
  # Failing this without failing empty would localize the bug to the
  # chunk store; failing both would localize to the composition /
  # Raft delta path.
  @integration @multi-node @nfs
  Scenario: 6-node cluster create-and-write non-empty file via NFSv4 succeeds
    Given a 6-node kiseki cluster
    When a client opens-creates-and-writes "hello-multi-node" via NFSv4 on node-1
    Then the NFSv4 COMPOUND status is NFS4_OK
    And a composition id is returned in the GETFH reply

  # pNFS wire-protocol coverage. The GCP 2026-05-02 perf-cluster
  # mount attempt failed with `nfs4: Unknown parameter 'pnfs'` from
  # the kernel — that's a kernel-side mount-option problem, not a
  # server-side bug. This scenario asserts the SERVER side works
  # over the wire on a multi-node cluster: a LAYOUTGET COMPOUND
  # against a real composition returns a Flexible Files layout
  # whose device addrs reference all 6 nodes' DS ports. Failure
  # means MdsLayoutManager isn't routing through the per-shard
  # storage_ds_addrs list, or the per-node DS port isn't bound, or
  # the NFSv4 LAYOUTGET op itself is broken — all of which would
  # silently break pNFS even after the kernel-mount issue is fixed.
  @integration @multi-node @nfs
  Scenario: 6-node cluster — NFSv4 LAYOUTGET returns layout addressing every DS
    Given a 6-node kiseki cluster
    When a 1KB object is PUT via S3 to node-1
    And a client issues NFSv4.1 LAYOUTGET against node-1 for that composition
    Then the LAYOUTGET reply is NFS4_OK
    And the returned layout references all 6 node DS addresses

  # End-to-end pNFS read: LAYOUTGET → GETDEVICEINFO → connect to a
  # DS uaddr → EXCHANGE_ID + CREATE_SESSION on the DS → PUTFH(fh) +
  # READ. This is the path the Linux kernel pNFS client takes after
  # mount, and what GCP would exercise once the mount-option issue
  # is resolved. The earlier scenario stops at GETDEVICEINFO; this
  # one closes the loop by actually reading bytes through the DS.
  @integration @multi-node @nfs
  Scenario: 6-node cluster — pNFS read fetches bytes from a DS
    Given a 6-node kiseki cluster
    When a 1KB object is PUT via S3 to node-1
    And a client issues NFSv4.1 LAYOUTGET against node-1 for that composition
    And the client opens a session to a DS from the layout and reads the first stripe
    Then the bytes returned by the DS match the original PUT body
