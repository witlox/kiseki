# Raft Consensus

Kiseki uses Raft for ordering and replicating deltas within each shard.
The implementation is based on openraft 0.10 with a custom TCP transport
and redb-backed persistent storage.

---

## Per-shard Raft groups

Each shard runs an independent Raft group (ADR-026, Strategy A). This
provides:

- **Independent scaling**: shard count grows with data volume and throughput
- **Isolated failure domains**: quorum loss in one shard does not affect
  others
- **No cross-shard coordination**: cross-shard rename returns EXDEV (I-L8)

The system key manager also runs its own Raft group for high availability
(ADR-007), as do audit log shards (ADR-009).

---

## openraft integration

The `kiseki-raft` crate defines `KisekiTypeConfig` used by all Raft groups:

- **Node identity**: `u64` node IDs
- **Async runtime**: tokio
- **Log store**: `RedbRaftLogStore` (persistent) or `MemLogStore` (testing)
- **Entry format**: customized per context (log deltas, key manager ops,
  audit events)

Each context (log, key manager, audit) defines its own request (`D`) and
response (`R`) types while sharing the node identity, entry format, and
async runtime configuration.

---

## Persistent log: RedbRaftLogStore

Raft log entries are persisted using `redb` (ADR-022), a pure-Rust
embedded key-value store. The `RedbRaftLogStore` provides:

- Durable append and truncation of log entries
- Vote persistence (current term, voted-for)
- Snapshot metadata storage
- Crash-safe operations (redb uses write-ahead logging internally)

For shards with inline data (ADR-030), the state machine offloads inline
content to `small/objects.redb` on apply. The in-memory state machine does
not hold inline content after apply (I-SF5).

---

## Snapshot transfer

When a follower falls behind or a new voter joins the group, the leader
sends a full snapshot:

1. Leader serializes the current state machine as length-prefixed JSON
2. For shards with inline data, the snapshot includes all entries from
   `small/objects.redb`
3. The snapshot is streamed over the Raft transport connection
4. The follower installs the snapshot and resumes normal replication

---

## Transport and security

Raft RPCs use a custom TCP transport with mTLS:

- All Raft communication is authenticated via per-node mTLS certificates
  signed by the Cluster CA (I-Auth1)
- The transport runs on the data fabric (not the management network)
- Connection pooling and keepalive are managed by the transport layer

The Raft transport address is configured via `KISEKI_RAFT_ADDR`.

---

## Dynamic membership changes

Raft membership changes follow the standard joint-consensus protocol:

1. **Add voter**: new node starts as learner, catches up to committed
   index, then promoted to voter
2. **Remove voter**: validated that removal does not break quorum
   (safety check via `can_remove_safely`)
3. **Shard migration**: target node must fully catch up (learner state
   matches leader's committed index) before old voter is removed (I-SF3)

Membership changes are validated by `validate_membership_change` in
`kiseki-raft`, which checks quorum preservation and prevents unsafe
removal.

---

## Shard lifecycle

| Event | Description |
|---|---|
| Create | New shard created when a namespace is created |
| Split | Mandatory split when shard exceeds ceiling (I-L6): delta count, byte size, or throughput |
| Maintenance | Shard set to read-only; writes rejected with retriable error (I-O6) |
| Compaction | Header-only merge; tenant-encrypted payloads carried opaquely (I-O2) |
| GC | Delta garbage collection after all consumers advance past the delta (I-L4) |

Shard splits do not block writes to the existing shard during the split
operation (I-O1).

---

## Consistency guarantees

| Scope | Guarantee | Mechanism |
|---|---|---|
| Intra-shard | Total order | Raft sequence numbers |
| Cross-shard | Causal ordering | HLC (Hybrid Logical Clock) |
| Cross-site | Eventual consistency | Async replication via federation |
| Writes | CP (no split-brain) | Raft majority commit (I-CS1) |
| Reads | Bounded staleness | Per view descriptor, subject to compliance floor (I-CS2) |
