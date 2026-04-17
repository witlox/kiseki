# Kiseki — Design Conversation (Distilled)

This is the source material for the analyst. It captures the extended design
conversation that converged on the Kiseki architecture. It is NOT a spec — it
is context. The analyst interrogates from here.

The conversation covered roughly sixteen turns and made approximately fourteen
substantive architectural commitments. Tensions and open questions are
preserved rather than resolved, because they are exactly what the analyst
needs to surface.

**Late addendum**: after the initial handoff package was assembled, the
domain expert surfaced that **encryption is a first-class citizen** —
not an optional feature, not a tier-specific choice, but an
architectural pillar. This has been folded into the conversation below
(section 2.15) and into SEED.md, but the fact that it emerged late is
itself worth noting: the analyst should probe whether other
architectural pillars are similarly implicit but unstated.

---

## 1. Origin and framing

The project began as an intellectual-itch investigation — explicitly mode (c)
in the earlier triage between (a) operational need, (b) strategic bet, and
(c) design exploration. The initial question was whether a VAST-class open
source distributed storage system tailored for Slingshot networks could be
built without the licensing costs or vendor lock-in.

The concrete substrate available is a pool of HPE Cray **ClusterStor E1000
and E1000F** enclosures that are to be repurposed. All-NVMe, Slingshot-
attached, running Lustre today. Lustre is to be removed — stated reasons:
unreliability, weak multi-tenancy, MDS bottleneck class of problems.

Stated non-goals:

- Not trying to out-compete VAST on peak performance
- Not trying to serve classical HPC with heavy MPI-IO small-block random
  writes as the primary workload
- Not trying to be hardware-dependent on ClusterStor specifically — it is
  the current substrate but the system should run on commodity nodes
- Not trying to solve everything; workload fit is deliberately bounded

Stated goals:

- Open source
- Software-first
- NFS and S3 as the baseline access protocols
- Multi-tenancy as fundamental (not bolt-on)
- Encryption at-rest and in-flight
- High performance for HPC and AI workloads, where those workloads are
  dominated by large sequential reads (data loading), bulk writes
  (checkpoints), and object-style access rather than random small writes
- Ease of operation — minimal management overhead for admins
- API-driven composability

---

## 2. Core architectural commitments

The following commitments were made through the conversation. Some are firm,
some are load-bearing but under-examined. The analyst should probe all of
them.

### 2.1 Log-as-truth

The system of record is an ordered, replicated log of **deltas**. Every
mutation is a delta appended to a log. The log, not any materialized state,
is authoritative.

This was arrived at through: write path simplification (no COW cascades
through deduplicated chunks), consistency establishment at one layer
(everything downstream inherits log consistency), and separation of
durability from access (truth is durable; projections are rebuildable).

### 2.2 Chunk layer

Data at rest is stored as **chunks** — opaque, content-addressable or
UUID-keyed segments. Chunks have no inherent "file" or "object" identity.
Placement is affinity-aware (chunks can prefer specific node classes) but
chunks do not carry protocol semantics.

Chunking strategy was discussed but not decided. Content-defined chunking
via Rabin-like fingerprinting was suggested as the default; similarity-based
chunking (VAST-style) was named as more complex and probably out of scope
for v1.

### 2.3 Composition layer

A **composition** is a metadata structure describing how to assemble chunks
into a coherent data unit with specific access semantics. A POSIX file is
one kind of composition. An S3 object is another. Compositions reference
chunks; chunks can be referenced by multiple compositions (which is where
dedup emerges).

Compositions are tenant-scoped. Chunk sharing across tenants is a per-tenant
policy decision (opt-in for efficiency, opt-out for isolation/compliance).

### 2.4 Materialized views

Reads are served from **materialized views**, not from the log directly in
the common case. Views are projections of the log, shaped for specific
access patterns and access protocols.

Views are:
- Rebuildable from the log at any time
- Independently tiered (hot views on fast tier, cold on bulk, discarded views
  on demand)
- Multiple per underlying composition (NFS view, S3 view, sequential-scan
  view, random-access view could all coexist over the same underlying data)
- Maintained incrementally by stream processors consuming the log

Read-write consistency across views is an acknowledged open problem — the
conversation settled on "the view descriptor declares the semantics; the
protocol translator enforces hard constraints at the boundary."

### 2.5 Sharded log with Raft per shard

The log is sharded by consistency set — roughly the smallest unit that needs
internal total ordering. For POSIX views, probably per-namespace or per-
top-level-subtree. For S3, per-bucket. Each shard has its own Raft group
(probably 3 members for most shards, 5 for high-criticality). Leaders are
distributed across nodes (multi-Raft pattern, à la TiKV/CockroachDB).

Cross-shard operations require 2PC or a transaction coordinator. The
expectation is that cross-shard operations are infrequent and appropriately
expensive.

The log storage uses the same tiered storage as everything else — hot
segments on fast tier, old segments on bulk. No special hardware for the
log.

### 2.6 Protocol and transport pluggability (N×M)

The gateway layer is factored as **protocol × transport**, with each being
a plugin. A protocol plugin (NFSv4.1 state machine, S3 API semantics)
composes with a transport plugin (TCP, RDMA verbs, libfabric/CXI). Target
is Level 2 in the conversation's taxonomy — protocol and transport as
separate plugins, composable.

### 2.7 Topology polymorphism

The system must run in three deployment topologies:
- **Dedicated** — storage nodes on their own fabric (VAST-style)
- **Shared** — storage and compute on same fabric (GPFS-style)
- **Hyperconverged** — storage daemons run alongside compute on the same
  nodes (Ceph-style)

This is a deployment-time choice, not an architecture-time choice. The
architecture must genuinely stay topology-neutral; design decisions that
implicitly assume a topology need to be flagged.

### 2.8 Auto-discovery + admin policy

The system auto-discovers physical reality (fabric, devices, tiers, peer
capabilities) and accepts admin-provided intent/policy (tenants, SLOs,
which flavors to expose). Recommendation of deployment "flavors" is
rules-based, explainable, and overridable. No ML.

### 2.9 Native client does the work, workload stays stupid

The design assumes workloads and schedulers are uncooperative — they will
not declare access patterns, hint, or coordinate with the storage system.
The intelligence therefore lives client-side in a native client library:
- FUSE path for POSIX-compatibility with minimal install friction
- Library path for performance (linkable as a native library, with C FFI,
  Python and C++ bindings produced as wrappers)
- No kernel modules, no reboot required
- Client detects access patterns and selects appropriate view/materialization
- Client uses best available transport (libfabric/CXI, then verbs, then TCP)
- Clients that can't run the native library use standard NFS/S3 gateways

### 2.10 Affinity pools, not rigid partitioned pools

Devices self-classify into affinity groups by type (fast-NVMe, bulk-NVMe,
etc.). Placement policies compose across these groups. A namespace can have
its metadata on fast tier and its data on bulk tier and still be one logical
thing. This is closer to CRUSH's device classes than to Ceph-style rigid
pools.

### 2.11 Multi-tenancy includes service placement

Tenancy is not a control-plane-only concept. A tenant's protocol gateways,
metadata shards, and QoS enforcers land on specific nodes based on policy
(locality, isolation, tier affinity). Services go where the data is.

Density vs. isolation is an open trade-off. Whether small tenants share
services with hard QoS, or every tenant gets dedicated services, was not
resolved.

### 2.12 Tiering is primarily declarative, with bounded reactive behavior

The view descriptor declares the baseline tier. Reactive access-pattern
driven promotion/demotion operates within bounds declared by the descriptor.
Pure reactive tiering is explicitly rejected as the "auto-tiering
graveyard." Views that are cold can be discarded and rebuilt from the log;
raw data cannot be discarded.

### 2.13 Language split: Rust core + Go control plane

- Rust for the core (log, chunks, views, native client, hot paths)
- Go for the control plane (declarative API, operators, CLI, recommendation
  advisor)
- gRPC/protobuf as the boundary between them
- Python/C/C++ bindings for the native client, produced as wrappers

### 2.14 Scope: what's in v1

Single-tenant was floated then rejected — multi-tenancy is fundamental from
the start, though it can be logical rather than requiring a full service
mesh on day one.

One wire protocol and one transport as v1 would be a reasonable first cut
(S3 over TCP was mentioned as easiest), with libfabric and NFS added in
subsequent passes. This was suggested but not committed.

### 2.15 Encryption as a first-class citizen (late addition)

Surfaced after the initial handoff package was drafted. The commitment:
encryption is not a feature, not a tier-specific option, not a
per-tenant opt-in. It is architectural.

Working interpretation (unvalidated — the analyst must confirm the
exact semantics):

- **All data at rest is encrypted.** Chunks on disk are never in
  plaintext. No "encryption off" mode.
- **All data in flight is encrypted.** Wire traffic between clients
  and servers, between servers, and for log replication is
  encrypted. Including the Slingshot/RDMA path, which introduces
  non-trivial trade-offs with one-sided operations (see tension
  section below).
- **All metadata (logs, view state) is encrypted.** The log carries
  filenames, directory structure, access patterns, and sizes — all
  of which leak substantially if plaintext. First-class encryption
  implies the log's payloads are encrypted too. Raft replicates
  ciphertext it cannot introspect.
- **Keys are tenant-scoped and tenant-controlled.** A tenant's
  keys are theirs. The system can enforce access and serve
  ciphertext; it cannot read plaintext without tenant-provided
  unwrapping. This implies the native client and protocol gateways
  may do the actual encrypt/decrypt, with the storage substrate
  handling only ciphertext.
- **Crypto-shred is the deletion primitive.** To delete, destroy
  the key. The remaining ciphertext is unreadable. This is how
  deletion works reliably in a distributed system where data is
  replicated, cached, backed up, and possibly on offline media.
  Actual chunk GC still happens, but delete-by-key-destruction is
  the semantically authoritative action.
- **Key management is not an afterthought.** KMS, key rotation,
  key escrow or split-key recovery, HSM integration, KMIP
  compatibility, envelope encryption — these are all part of the
  system, not bolted on.

**Implications surfaced immediately:**

1. **Cross-tenant dedup becomes problematic.** Global dedup
   requires convergent encryption (same plaintext → same
   ciphertext), which is a known side-channel (it leaks that two
   tenants have the same data). Intra-tenant dedup with per-tenant
   keys works cleanly. This probably forces intra-tenant-only
   dedup as the default. The VAST-style global-dedup efficiency
   story doesn't translate directly.

2. **Slingshot RDMA one-sided operations vs. encryption.** One-
   sided RDMA reads bypass the target CPU by design. Wire
   encryption requires *someone* to encrypt/decrypt — CPU
   (expensive, defeats the one-sided point), SmartNIC offload
   (Cassini has some capability; libfabric exposure is uncertain),
   or pre-encrypted blocks on wire with client-side decryption
   (works if the encryption unit aligns with the RDMA transfer).
   This needs to be designed, not wished away.

3. **Observability under encryption is constrained.** Ops can see
   that access happened and can see envelope metadata, but
   cannot see payloads. Debugging tenant-reported issues becomes
   harder. This needs to be explicit in the operational contract.

4. **Key loss is data loss.** A tenant that loses their keys has
   lost their data. Escrow, split-key recovery, or organization-
   level master keys are policy choices that need to be made.

5. **Key management is a bounded context.** Probably a seventh,
   alongside log / chunks / compositions / gateways / clients /
   control plane. Its failure modes and availability contract
   are load-bearing.

6. **Line-rate encryption is not free.** At 200+ Gbps per NIC,
   AES-NI on CPU consumes meaningful cycles. The design needs to
   be explicit about where encryption happens (client? gateway?
   NIC offload?) and what the performance envelope is.

Not yet specified:
- Algorithm choices (AES-GCM is the obvious candidate, but ChaCha20-
  Poly1305, AES-GCM-SIV, XChaCha20 all have trade-offs)
- Envelope structure (per-chunk keys wrapped by per-tenant keys
  wrapped by master keys?)
- Key rotation semantics (does rotation require re-encryption of
  existing data, or re-encryption of envelope keys only?)
- Authenticated encryption vs. encryption-plus-MAC
- TLS vs. custom wire protocol for encrypted transport
- Whether compression-before-encryption is allowed (it opens
  CRIME/BREACH-style side channels)
- Integrity verification and what's authenticated

---

## 3. Tensions that were not fully resolved

These are not bugs in the design — they're the remaining design work. The
analyst should treat them as priority probe points.

### 3.1 "No master node" vs. Raft leadership

The original instinct was "no master node." The landed position is
"per-shard Raft with leaders distributed across nodes." These are
reconciled by saying the Raft leader is per-shard (not system-wide) and
leadership rotates on failure. But it's worth asking: is this actually
the property that was wanted, or is there an underlying desire (e.g.,
no operator-visible "metadata node" role) that Raft-per-shard doesn't
fully satisfy?

### 3.2 Log as truth vs. metadata-corruption failure modes

The conversation flagged that log-based architectures are *more*
dependent on log integrity than chunk-based ones — a corrupted log has a
larger blast radius than a corrupted metadata store. This was acknowledged
but not designed for. Log integrity guarantees, verification mechanisms,
and recovery procedures are underspecified.

### 3.3 Cross-view consistency

When the NFS view and the S3 view of the "same" data are maintained
independently by different stream processors, they can diverge in time.
The conversation settled on "declare the semantics in the view descriptor"
but the actual semantics (read-your-writes across protocols? bounded
staleness? eventual?) were not specified.

### 3.4 Dedup scope and GC

Chunk-level dedup was mentioned as emergent from composition-over-chunks.
Distributed reference counting for GC was named as a famously hard problem
with multiple approaches (mark-and-sweep, live refcounts, epoch-based).
No choice was made.

Per-tenant dedup policy (dedup within tenant only vs. across tenants) was
named as configurable. The mechanism for enforcing this was not specified.

### 3.5 Write path for POSIX mutations

Log-structured write path is clean for S3 (immutable PUT). For POSIX
byte-range writes, the story is: "it's a small delta, applied at the
composition level." This works conceptually but the composition-level
delta representation for POSIX semantics (byte ranges, sparse files, mmap,
atomic renames, hardlinks) was not specified.

### 3.6 Compaction at scale

LSM-style compaction is the implicit read-side strategy. Compaction is
where log-structured systems live or die operationally. Online compaction,
compaction scheduling under sustained write load, distributed compaction
across shards, interaction between compaction and view materialization —
all flagged but not designed.

### 3.7 Density of per-tenant services

If every tenant has dedicated Ganesha + S3 gateway + metadata shards, how
many tenants fit on the hardware? Shared-but-isolated vs. dedicated-per-
tenant was named but not resolved.

### 3.8 Small-write workloads

Log-structured systems are fast for bulk writes and acceptable for random
writes via deltas, but POSIX workloads with heavy small-block random I/O
will stress the system. The conversation's position is "architect to
refuse" — be explicit that this isn't the target workload — rather than
"architect to tolerate." Whether that's acceptable needs to be confirmed
by the domain expert.

### 3.9 Encryption vs. cross-tenant dedup

First-class encryption with per-tenant keys and cross-tenant dedup are
in direct tension. Convergent encryption (same plaintext → same
ciphertext) enables cross-tenant dedup but leaks co-occurrence of data
across tenants, which is a recognized side-channel. Per-tenant keys
with no cross-tenant dedup is clean but loses the global-dedup
efficiency argument. The design needs to pick, and the analyst needs
to force the pick.

### 3.10 Encryption vs. one-sided RDMA on Slingshot

One-sided RDMA reads bypass the target CPU. Wire encryption requires
encryption/decryption somewhere. Resolving this tension requires one
of: CPU involvement on the target (defeats the one-sided point),
SmartNIC offload (depends on Cassini NIC capabilities and libfabric
provider support — both uncertain), or storing pre-encrypted blocks
that can be fetched via one-sided ops and decrypted client-side
(works when encryption unit aligns with RDMA transfer). The design
has not addressed this.

### 3.11 Key lifecycle and crypto-shred semantics

If crypto-shred is the deletion primitive, key destruction must be
reliable and auditable. Questions the design has not answered:
- How is key destruction confirmed across replicas and backups?
- What happens when a tenant rotates a key — is existing data
  re-encrypted (envelope re-wrap) or only new data?
- Is there key escrow for organization-level recovery? If yes, what
  breaks the "tenant controls their keys" property?
- How does crypto-shred interact with compliance requirements that
  mandate retention (e.g., litigation hold)?

### 3.12 Key management as a bounded context

The design conversation treated key management implicitly (mentioned
"encryption" as a goal but did not specify where keys live, who
manages them, how they rotate, or how they integrate with the rest
of the system). Making encryption first-class probably requires
treating key management as its own bounded context with its own
availability contract, failure modes, and integration points. The
analyst should confirm this framing with the domain expert.

---

## 4. Prior art identified

### 4.1 DeltaFS (CMU PDL / LANL)

Extremely close architectural overlap. Per-job log-structured metadata,
serverless (metadata services instantiated on compute nodes per job),
LSM-tree-based log format, namespace snapshots as immutable log
references, "No Ground Truth" principle (no global synchronized namespace).

Key papers:
- Zheng et al., "DeltaFS: Exascale File Systems Scale Better Without
  Dedicated Servers" (PDSW 2015)
- Zheng et al., "DeltaFS: A Scalable No-Ground-Truth Filesystem for
  Massively-Parallel Computing" (SC 2021)
- Zheng PhD dissertation CMU-CS-21-103 (2021)
- Multiple IMD papers (2017, 2018, 2020) on Indexed Massive Directories

See `docs/prior-art/deltafs-mochi-evaluation.md` for detailed comparison.

### 4.2 Mochi (Argonne / LANL / CMU / HDF Group)

Composable HPC data services framework. Provides building blocks
(Mercury/Margo/Thallium for RPC, Bake for BLOB storage, SDSKV for KV,
SSG for group membership, REMI for migration) that compose into
specialized services. Focuses on rapid development of domain-specific
services rather than one-size-fits-all filesystems.

Directly relevant to the "N×M pluggable" and "protocol × transport"
commitments. Mochi is a possible substrate to build on or learn from
rather than duplicate.

### 4.3 Other named-but-not-deeply-examined prior art

- Lustre (base system being replaced)
- Ceph (CRUSH placement, BlueStore, reference for multi-protocol unified
  storage)
- DAOS (post-Optane, open source, libfabric-native, aimed at HPC/AI)
- GPFS / Spectrum Scale (converged fabric model)
- VAST (DASE disaggregation, global dedup, ultra-wide EC, similarity-based
  chunking)
- Weka (shared services with aggressive isolation)
- TiKV / CockroachDB (multi-Raft patterns)
- FoundationDB (distributed KV with transactional semantics)
- Kafka / Pulsar / BookKeeper (log systems)
- CORFU / Delos (log-as-database pattern, academic)
- CRAQ (chain replication with apportioned queries)
- Differential Dataflow / Materialize (incremental view maintenance)
- CQRS / Kappa architecture (event sourcing patterns)

---

## 5. Name, language, deployment

- **Project name**: Kiseki (軌跡) — Japanese: locus, trajectory, trace.
  Checked against software projects; no collisions found. Cultural
  association with "The Legend of Heroes: Kiseki" JRPG series exists; this
  is not expected to be a problem for a storage system.
- **Core language**: Rust
- **Control plane language**: Go
- **Boundary**: gRPC over protobuf
- **Client bindings**: Rust native + C FFI, Python via PyO3 or similar,
  C++ wrapper

---

## 6. Where the conversation left off

The design is coherent at the level of architectural commitment. It is
unspecified at the level of entity definitions, invariants, behavioral
contracts, failure modes, and integration boundaries. That is exactly the
work the analyst does.

The following should not be assumed settled, even though they were
discussed:

- Chunking strategy (fixed / content-defined / similarity-based)
- Metadata KV backend (FoundationDB, TiKV, or bespoke LSM)
- POSIX semantics depth (full POSIX vs. POSIX-subset)
- Consistency model for cross-view reads
- v1 scope (which protocols, which transports, which topology modes)
- Tenancy density model (shared services vs. dedicated)
- Compaction strategy
- Recovery and verification for corrupted logs

---

## 7. Explicit non-goals worth restating

- Not competing with VAST on peak performance
- Not a research project — existing patterns should be used where they apply
- Not "all things for all workloads" — the architecture is honest that
  small-random-write workloads are not the target
- Not inventing consensus — Raft via a mature library (openraft, etcd-raft,
  hashicorp/raft) is the assumption
- Not inventing a KV store — FoundationDB/TiKV are the expected metadata
  backends unless there's a specific reason otherwise
