# Cross-Context Interactions — Kiseki

**Status**: Layer 4 — interrogation in progress.
**Last updated**: 2026-04-17, Session 2.

---

## End-to-end data paths

### Write path: NFS client → storage (6 contexts)

```
NFS Client
  │ plaintext over TLS
  ▼
Protocol Gateway (encrypt, chunk)
  │ encrypted chunks    │ delta (header + encrypted payload)
  ▼                     ▼
Chunk Storage        Composition
  │ ChunkStored         │ AppendDelta
  │                     ▼
  │                   Log (Raft commit)
  │                     │ DeltaCommitted
  │                     ▼
  │                View Materialization (stream processor consumes)
  │
  └─── Key Management (system DEK for chunk encryption,
                        tenant KEK wrapping for access)
```

**Contract**: The Protocol Gateway is the coordinator. It:
1. Receives plaintext from the NFS client over TLS
2. Chunks the plaintext (content-defined, variable-size)
3. Computes chunk_ids (sha256 or HMAC per tenant dedup policy)
4. Writes encrypted chunks to Chunk Storage (idempotent)
5. Waits for ChunkStored confirmations
6. Submits delta to Composition context
7. Composition appends delta to Log (Raft commit)
8. Waits for DeltaCommitted
9. Returns success to NFS client

**Failure at any step**:
- Step 4 fails (chunk write): abort, return retriable error, no delta committed
- Step 7 fails (delta commit): abort, orphan chunks GC'd (refcount 0)
- Step 8 timeout: client retries; idempotent chunk writes safe to repeat

**Key material flow**:
- Gateway holds cached tenant KEK (from tenant KMS)
- Gateway requests system DEK from system key manager
- System DEK encrypts chunk; system DEK wrapped with tenant KEK in envelope

---

### Write path: Native client → storage (5 contexts)

```
Workload Process
  │ plaintext (in-process)
  ▼
Native Client (encrypt, chunk — all in-process)
  │ encrypted chunks    │ delta
  ▼                     ▼
Chunk Storage        Composition → Log
  │                     │
  └─── Key Management   └─── View Materialization
```

**Contract**: Same as gateway path, but encryption happens in the
workload process. Plaintext never leaves the process. The native client
is the coordinator.

**Key difference from gateway path**: no TLS-protected plaintext on the
wire. The native client encrypts before any network I/O. This is the
strongest security posture — the storage infrastructure never sees
plaintext.

---

### Read path: NFS client ← storage (4 contexts)

```
NFS Client
  ▲ plaintext over TLS
  │
Protocol Gateway (decrypt)
  ▲ encrypted chunks    ▲ view state (decrypted metadata)
  │                     │
Chunk Storage      View Materialization
  │
  └─── Key Management (tenant KEK → unwrap system DEK → decrypt)
```

**Contract**: The Protocol Gateway:
1. Resolves path in the NFS view (view already has decrypted metadata
   — stream processor decrypted during materialization)
2. Identifies chunk references for the requested byte range
3. Fetches encrypted chunks from Chunk Storage
4. Unwraps system DEK via tenant KEK
5. Decrypts chunks to plaintext in gateway memory
6. Returns plaintext to NFS client over TLS
7. Discards plaintext from memory

**MVCC**: The read pins the view at a specific watermark (log position).
Concurrent writes after the pin are invisible. Pin has bounded TTL.

---

### Read path: Native client ← storage (3 contexts)

```
Workload Process
  ▲ plaintext (in-process)
  │
Native Client (decrypt in-process)
  ▲ encrypted chunks (may be one-sided RDMA)
  │
Chunk Storage ← Key Management
```

**Contract**: Same as gateway read but decryption is in-process.
For Slingshot/RDMA: one-sided read transfers ciphertext directly to
client memory; client decrypts using cached keys. Storage node CPU
not involved.

---

## Context-to-context contracts

### Log → View Materialization

**Interface**: Delta stream (ReadDeltas range query).

**Contract**:
- Stream processors consume deltas sequentially from their last watermark
- Deltas are delivered in total order per shard
- Stream processors decrypt delta payloads using cached tenant KEK
- Watermark is persisted durably by the stream processor
- If the stream processor crashes, it resumes from last persisted watermark
- Idempotent application: replaying a delta produces the same view state

**Availability**: If the shard is unavailable, the stream processor stalls.
The view serves last-known state. Reads are marked as potentially stale.

---

### Composition → Log

**Interface**: AppendDelta command.

**Contract**:
- Composition submits delta with header (system-visible) + payload (tenant-encrypted)
- Log assigns sequence number and replicates via Raft
- Log returns DeltaCommitted with sequence number
- If shard is unavailable: retriable error to Composition, which propagates to gateway/client

**Ordering**: Composition ensures chunk writes complete before submitting
delta (normal path). For multipart: finalize delta committed only after
all chunks confirmed durable (I-L5).

---

### Composition → Chunk Storage

**Interface**: WriteChunk command, refcount management.

**Contract**:
- Composition submits plaintext for chunking and encryption
- Chunk Storage encrypts with system DEK and stores
- Returns chunk_id and ChunkStored confirmation
- Composition manages refcounts: increment on reference, decrement on dereference
- Chunk Storage enforces GC rules (refcount 0 + no hold)

**Idempotency**: WriteChunk with same chunk_id is idempotent (dedup).
Second write increments refcount only.

---

### Key Management → Chunk Storage

**Interface**: System DEK provisioning.

**Contract**:
- Chunk Storage requests system DEK for encryption
- Key Management provides DEK from current epoch
- DEK is wrapped with system KEK in the envelope
- If Key Management is unavailable: chunk writes fail (cluster-wide write outage)

---

### Key Management → Protocol Gateway / Native Client

**Interface**: Tenant KEK provisioning.

**Contract**:
- Gateway/client requests tenant KEK from tenant KMS
- Key Management facilitates (or the component contacts tenant KMS directly)
- KEK is cached with bounded TTL
- If tenant KMS is unavailable: cached KEK sustains operations within TTL
- After TTL: operations fail for that tenant

---

### Control Plane → All contexts

**Interface**: Policy, placement, tenant config (pull-based).

**Contract**:
- Contexts pull configuration from the Control Plane
- Changes propagate on next poll cycle (eventually consistent)
- If Control Plane is unavailable: contexts use last-known cached config
- No context depends on real-time Control Plane availability for the data path

---

### View Materialization → Protocol Gateway / Native Client

**Interface**: View read queries.

**Contract**:
- Gateway/client reads from the materialized view
- View provides MVCC snapshot at a specific watermark
- The view's consistency model (read-your-writes, bounded-staleness) is
  declared in the view descriptor
- If the view is stale beyond its bound: reads may return stale-data warning
- If the view is discarded: reads fail until the view is rebuilt

---

## Cross-context failure cascades

### System key manager failure (highest severity)

```
System Key Manager DOWN
  → Chunk Storage: cannot encrypt new chunks → all writes fail
  → All gateways: writes rejected (retriable)
  → All native clients: writes rejected (retriable)
  → Reads: may continue using cached system DEKs (bounded TTL)
  → Alert: cluster admin, highest severity
  → Recovery: restore system key manager quorum
```

### Tenant KMS failure (tenant-scoped)

```
Tenant KMS unreachable for org-pharma
  → Gateway pharma: cached KEK sustains reads/writes within TTL
  → Native clients pharma: same
  → Stream processors pharma: same
  → After TTL: all org-pharma operations fail
  → Other tenants: unaffected
  → Alert: tenant admin + cluster admin
  → Recovery: restore tenant KMS connectivity
```

### Shard quorum loss (shard-scoped)

```
Shard "shard-trials-1" loses quorum
  → Composition: writes to this shard fail (retriable)
  → Stream processors: stall at last watermark
  → Views: serve last-known state (stale)
  → Reads: succeed but potentially stale
  → Writes: fail for namespaces in this shard
  → Other shards: unaffected
  → Alert: cluster admin + affected tenant admins
  → Recovery: restore shard quorum (node recovery or reconfiguration)
```

### Control Plane failure (degraded but operational)

```
Control Plane DOWN
  → Data path: continues with last-known cached config
  → Tenant management: blocked (no new tenants, no policy changes)
  → Shard creation: blocked (no new namespaces)
  → Quota enforcement: approximate (cached values, drift possible)
  → Federation: config sync stalls
  → Alert: cluster admin
  → Recovery: restore Control Plane; reconcile quota drift
```

---

## Anti-patterns to avoid

1. **Gateway calling Log directly.** Writes must go through Composition
   (refcount management, namespace validation, chunk-before-delta ordering).
2. **Stream processor writing to Log.** Stream processors are read-only
   consumers of the log. They write to view state only.
3. **Native client contacting Control Plane on the data path.** Discovery
   and config are bootstrapped/cached. The data path uses fabric-level
   discovery.
4. **Chunk Storage decrypting payloads.** Chunk Storage stores and
   retrieves opaque ciphertext. Decryption is the caller's responsibility
   (gateway, client, stream processor).
5. **Cross-shard coordination on the write path.** Shards are independent.
   Cross-shard operations return EXDEV.
