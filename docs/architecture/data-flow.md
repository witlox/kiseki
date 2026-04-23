# Data Flow

This page describes the write, read, inline, and cross-node data paths
through the Kiseki system.

---

## Write path

```
┌──────────┐    plaintext     ┌──────────────────┐
│  Client  │ ──────────────► │  Gateway /        │
│          │   (over TLS)    │  Native Client    │
└──────────┘                 └────────┬──────────┘
                                      │ 1. Encrypt with tenant KEK
                                      │    wrapping system DEK
                                      │ 2. Content-defined chunking
                                      │    (Rabin fingerprinting)
                                      ▼
                             ┌──────────────────┐
                             │   Composition    │
                             │                  │
                             │ 3. Record chunk  │
                             │    references    │
                             │ 4. Build delta   │
                             └───────┬──────────┘
                                     │
                            ┌────────┴────────┐
                            ▼                 ▼
                   ┌──────────────┐   ┌──────────────┐
                   │  Log (Raft)  │   │ Chunk Storage │
                   │              │   │               │
                   │ 5. Commit    │   │ 6. Write      │
                   │    delta via │   │    encrypted  │
                   │    Raft      │   │    chunk to   │
                   │ 7. Replicate │   │    device     │
                   │    to        │   │ 8. EC encode  │
                   │    majority  │   │    across     │
                   │              │   │    pool       │
                   └──────────────┘   └──────────────┘
```

### Step-by-step

1. **Client encrypt**: The native client encrypts data before it leaves
   the process. Protocol-path clients (NFS/S3) send plaintext over TLS
   to the gateway, which encrypts on their behalf.

2. **Content-defined chunking**: Data is split into variable-size chunks
   using Rabin fingerprinting. Each chunk gets a content-addressed ID
   (SHA-256 hash of plaintext, or HMAC when tenant opts out of cross-tenant
   dedup).

3. **Compose**: The composition layer records chunk references and
   constructs a delta describing the mutation (create, update, delete).

4. **Raft commit**: The delta is appended to the owning shard's Raft log.
   The leader replicates to a majority of voters before acknowledging.

5. **Chunk write**: Encrypted chunks are written to affinity pool devices
   with erasure coding (or N-copy replication, per pool policy).

6. **Ack**: The write is acknowledged to the client only after the delta
   is committed (I-L2) and all referenced chunks are durable (I-L5).

---

## Read path

```
┌──────────┐                 ┌──────────────────┐
│  Client  │ ◄────────────── │  Gateway /        │
│          │   plaintext     │  Native Client    │
└──────────┘   (over TLS)   └────────┬──────────┘
                                      ▲ 5. Decrypt
                                      │
                             ┌────────┴──────────┐
                             │   View Lookup     │
                             │                   │
                             │ 1. Resolve path   │
                             │    to composition │
                             │ 2. Get chunk list │
                             └────────┬──────────┘
                                      │
                                      ▼
                             ┌──────────────────┐
                             │  Chunk Storage   │
                             │                  │
                             │ 3. Read chunks   │
                             │    from device   │
                             │ 4. EC decode if  │
                             │    degraded      │
                             └──────────────────┘
```

### Step-by-step

1. **View lookup**: The client or gateway queries a materialized view to
   resolve a path (POSIX) or key (S3) to a composition and its chunk list.

2. **Chunk read**: Encrypted chunks are read from the storage devices.
   If a device is degraded, EC parity reconstructs the missing data.

3. **Decrypt**: The client (native path) or gateway (protocol path)
   unwraps the system DEK using the tenant KEK, then decrypts the chunk
   data with AES-256-GCM.

4. **Return**: Plaintext is returned to the client.

---

## Inline path (ADR-030)

Small files below the configurable inline threshold bypass chunk storage
entirely:

```
Client ──► Composition ──► Log (Raft)
                             │
                             ▼
                    Delta with inline payload
                             │
                             ▼
                    Raft replication to voters
                             │
                             ▼
                    State machine apply:
                    store in small/objects.redb
```

**Threshold computation**: The inline threshold for a shard is the minimum
affordable threshold across all nodes hosting that shard's voter set:

```
clamp(min(voter_budgets) / file_count_estimate, INLINE_FLOOR, INLINE_CEILING)
```

**Key invariants**:
- I-L9: Inlined payloads are immutable after write; threshold changes
  apply prospectively only
- I-SF5: Inline content is offloaded to `small/objects.redb` on state
  machine apply; snapshots include inline content from redb
- I-SF7: Per-shard Raft inline throughput capped at `KISEKI_RAFT_INLINE_MBPS`
  (default 10 MB/s)

---

## Cross-node data paths

### Raft replication

Each shard runs an independent Raft group (ADR-026). The leader replicates
log entries (deltas) to followers via the Raft RPC transport. Replication
uses mTLS on the data fabric.

```
Leader ──► Follower 1 (AppendEntries)
       ──► Follower 2 (AppendEntries)
       ──► Follower 3 (AppendEntries)
```

Committed entries are persisted in `RedbRaftLogStore` on each voter.

### Snapshot transfer

When a follower is too far behind or a new voter joins, the leader sends
a full snapshot. Snapshots are transferred as length-prefixed JSON over
the Raft transport connection.

For shards with inline data, the snapshot includes all entries from
`small/objects.redb` (I-SF5).

### Chunk replication and EC

Chunks are placed across distinct physical devices within a pool using
deterministic hashing (CRUSH-like). No two EC fragments of the same chunk
reside on the same device (I-D4).

Device failure triggers automatic repair from EC parity or replicas (I-D1).

### Federation

Federated sites replicate data asynchronously. Only ciphertext is
replicated -- no key material in the replication stream (I-CS3). All
federated sites for a tenant connect to the same tenant KMS.
