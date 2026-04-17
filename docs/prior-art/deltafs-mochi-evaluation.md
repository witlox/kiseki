# Prior Art Evaluation: DeltaFS and Mochi vs. Kiseki

**Purpose**: Identify architectural overlap, meaningful divergence, and
design problems that prior art has already surfaced. This is input to the
analyst, not a verdict — but it should substantially shape the
interrogation.

**Sources consulted**:
- Zheng et al., "DeltaFS: Exascale File Systems Scale Better Without
  Dedicated Servers", PDSW 2015
- Zheng et al., "DeltaFS: A Scalable No-Ground-Truth Filesystem for
  Massively-Parallel Computing", SC 2021 (CMU-PDL-21-101)
- Ross et al., "Mochi: Composing Data Services for High-Performance
  Computing Environments", JCST 2020
- PDL DeltaFS project page; Mochi project page; Mochi code index

---

## 1. Executive summary

Kiseki's architectural commitments are not independent — they converge on
the same design space that CMU PDL, Argonne MCS, and LANL HPC-DIV have
been actively exploring for over a decade. This is validation, not
discouragement: when independent groups with real exascale test harnesses
arrive at log-structured, serverless, per-job metadata with namespace
snapshots as immutable log references, the pattern is probably correct.

The honest question is what Kiseki does that DeltaFS and Mochi do not,
and the answer has three substantive parts:

1. **Multi-tenancy as a first-class concern**. DeltaFS is designed for
   HPC batch jobs that trust the cluster and do not coexist in the
   storage sense. Kiseki assumes adversarial or at least distrustful
   tenants.
2. **Multiprotocol access (NFS + S3) over the same substrate**. DeltaFS
   is a library-linked custom API; Mochi services are bespoke per
   application. Kiseki targets standard-protocol clients as the baseline.
3. **Persistent, non-job-scoped storage service**. DeltaFS filesystem
   instances are fundamentally per-job — they instantiate, serve, publish
   a snapshot, tear down. Kiseki is a persistent service.

These three differences are not small. They change what the metadata
service has to guarantee, how long state has to live, how access is
authorized, and what failure modes matter. The analyst should treat them
as the core differentiators that shape most downstream design decisions.

---

## 2. DeltaFS — detailed comparison

### 2.1 Where DeltaFS and Kiseki agree

**Log-structured metadata as the source of truth.** Identical. DeltaFS
calls these "change sets" — per-job logs of metadata mutations persisted
as SSTables in an underlying object store, indexed by a manifest object.
Kiseki's "log of deltas, shards, consistency sets" maps to essentially
the same structure. DeltaFS uses a modified LevelDB LSM-tree for the
in-memory and on-storage representation; Kiseki would follow the same
pattern.

**Namespace as composition of logs.** DeltaFS jobs construct their
namespace view by selecting input snapshots and merging them — with
priority ordering for conflict resolution (multi-inheritance DAG).
Kiseki's compositions are conceptually the same: "a composition is a
metadata structure describing how to assemble underlying state."

**Serverless metadata processing.** DeltaFS instantiates metadata
servers inside the compute nodes running the job itself. Kiseki proposes
per-tenant service placement that lands gateways on nodes based on
policy. Both avoid the dedicated-metadata-server bottleneck.

**Chunks/objects as opaque substrate.** DeltaFS writes file data to an
underlying object store (RADOS, PVFS, HDFS have all been backends).
Kiseki's chunk layer is the same abstraction.

**Parallel compaction by harvested compute.** DeltaFS's Compaction
Runners launch on compute nodes and run parallel merge sort across
SSTables. Kiseki's "view materialization by stream processors" is a
generalization of the same pattern.

**LSM with SSTables and per-key sequence numbers.** DeltaFS's log
entry format is KV pairs keyed by `(parent_dir_id, base_name, seq_num,
tombstone_bit)` with the value being the inode metadata. This is exactly
the format Kiseki's log would need. The sequence-number-with-tombstone
pattern handles updates, deletes, and merges cleanly.

### 2.2 Where Kiseki diverges meaningfully

**Persistence model.** DeltaFS is fundamentally a *transient* filesystem
service — each job instantiates it, publishes a snapshot at end, and
tears down. Communication between jobs is via published snapshots in a
registry. Kiseki is a *persistent* storage service that must serve
long-lived clients continuously.

This is a substantial architectural difference. DeltaFS avoids the
hardest failure-recovery problems by having each job own its metadata
lifecycle. Kiseki cannot — it has to keep logs healthy across node
failures, network partitions, and indefinite uptime.

**Multi-tenancy.** DeltaFS has no real tenancy model. Jobs are
self-managed and trust each other up to Unix permissions on the
underlying object store. Kiseki needs per-tenant namespaces, per-tenant
service isolation, per-tenant keys, per-tenant QoS, per-tenant dedup
scope, and per-tenant storage accounting. None of this is in DeltaFS.

**Protocol surface.** DeltaFS is accessed through a library API
(`deltafs_api.h`) or via LD_PRELOAD interception. There is no NFS, no
S3, no pNFS. Applications link DeltaFS or they don't use it. Kiseki
explicitly targets NFS and S3 as baseline, with native library as the
fast path. The protocol gateways are first-class infrastructure in
Kiseki and absent in DeltaFS.

**Concurrent read-write while writing.** DeltaFS snapshots are
finalized at job end. During a job's execution, the job itself sees
consistent state of its own writes (via intra-job metadata
synchronization) but other jobs do not see the writes until the
snapshot is published. Kiseki is expected to support concurrent
read-write access across clients — which means the consistency story
Kiseki needs is substantially more demanding than DeltaFS's "publish
snapshots at job boundaries."

**Registry vs. persistent metadata service.** DeltaFS has a
"Namespace Registry" — dedicated servers that map snapshot names to
their manifest objects. This is a small, simple service because
registries are not on the critical path (they are used for
discovery, not for serving reads/writes). Kiseki's metadata
coordination is on the critical path and needs different
availability properties.

**Encryption.** DeltaFS has essentially no encryption story. The
papers do not address it; the system relies on underlying object
store security (which, for RADOS-backed deployments, is at best
at-rest disk encryption and transport TLS). There is no tenant-
controlled key material, no crypto-shred, no encrypted log deltas,
no KMS integration. This is not a deficiency of DeltaFS — it was
designed for cooperative HPC workloads where encryption was not a
requirement — but it means Kiseki cannot inherit an encryption
design from DeltaFS. The encryption architecture is entirely
Kiseki's to design, and because encryption is first-class, it
shapes the log format, the chunk envelope, the wire protocol, the
gateway responsibilities, and the failure modes. None of this is
covered by the DeltaFS prior art.

### 2.3 Problems DeltaFS encountered that are relevant to Kiseki

These are the most valuable parts of the prior art — empirical findings
from running DeltaFS at scale on Trinity and other platforms.

**Write-ahead log failure recovery is non-trivial at scale.** DeltaFS's
per-partition WAL needs to survive a compute-node failure in the middle
of a job. They solve this by persisting the WAL to the underlying object
store before acknowledging the write to the client. This introduces
latency on every metadata write. Kiseki's durability story has to be
equally explicit.

**Background compaction competes with foreground writes.** DeltaFS notes
that when background compaction cannot keep up with foreground
insertion, client latency spikes because the system has to compact
synchronously to keep read amplification bounded. This is the
compaction-storm problem that every LSM-based system fights. Kiseki
inherits this problem. The DeltaFS paper's explicit framing: "when the
server compute resources are insufficient for the said workload, a
client still experiences delays." This is the tail-latency reality of
LSM-based storage.

**SSTable lookup cost grows linearly with unmerged SSTables per
partition.** DeltaFS queries SSTables from newest to oldest until a
match is found. Before compaction merges tables, a key not in the cache
requires searching N tables. Bloom filters help but don't eliminate the
cost. This is a known, bounded, but ever-present overhead.

**Parallel compaction requires separate compute allocation.** DeltaFS's
Compaction Runner is a separate program submitted to the job scheduler
— the user explicitly allocates compute to run compaction. This works
for HPC workflow scheduling but is exactly the kind of operational
burden Kiseki wants to avoid. Kiseki's compaction needs to be automatic
and resource-bounded, not manually scheduled, which means Kiseki has to
solve a problem DeltaFS side-stepped.

**Multi-inheritance DAG resolution has subtle edge cases.** DeltaFS's
priority-based name resolution (job D sees `/p/y` from B not C because
B is higher-priority) is clean in principle but creates hazards:
tombstones in lower-priority snapshots can shadow live entries in
higher-priority ones, renames across snapshots are ambiguous,
permission inheritance is context-dependent. The SC 2021 paper spends
significant space on these semantics. Kiseki's "composition over
multiple underlying sources" model has to handle similar semantics if
it supports any kind of snapshot or versioning.

**Registry scalability was deprioritized because registries are
off the critical path.** DeltaFS explicitly argues their registry can
be simple because it's used for discovery, not for serving I/O. Kiseki
cannot make this argument — its metadata layer IS on the critical path.
The scalability bar for Kiseki's equivalent is higher.

**Garbage collection of SSTables and data objects is refcount-based
and user-invoked.** DeltaFS provides a utility program
(`deltafs-changeset-clean`) that a user periodically runs. Deleted
change sets release references; data objects are deleted when all
SSTables referencing them are gone. Kiseki needs automatic GC on live
data. That's meaningfully harder.

**Metadata operations are ultimately bounded by job process count.**
DeltaFS achieves 98x speedup by harvesting per-process compute for
metadata. This works when the job has 10,000 processes. It does not
work for Kiseki's use case, where you might have one training job with
8 GPU nodes that wants millions of metadata ops/sec. Kiseki cannot
harvest client-side parallelism the way DeltaFS does, so absolute
metadata throughput per tenant is fundamentally smaller.

### 2.4 DeltaFS concepts Kiseki should reuse directly

- **Log entry format**: `(parent_dir_id, base_name, seq_num,
  tombstone_bit) → inode`. Avoids full-pathname keys, handles renames
  cheaply, supports range scans for directory listing. This is battle-
  tested over a decade.
- **Manifest-indexed SSTable sets** as the on-storage representation of
  a log shard. DeltaFS's manifest object lists SSTables, their key
  ranges, and dependencies. Kiseki's log shards can use the same layout.
- **Write-ahead log persisted to underlying object store** before ack.
  Standard durability pattern.
- **Background compaction with LRU for in-memory SSTable cache**.
  Standard.
- **Reference counting for GC of data objects**. The specific mechanism
  (each SSTable holds references, objects deleted when refcount hits
  zero) is appropriate for an LSM-based system. Kiseki can use this but
  needs to automate what DeltaFS leaves manual.

### 2.5 DeltaFS concepts Kiseki should NOT adopt

- **Job-scoped metadata service lifecycle**. Kiseki is persistent.
- **"Publish snapshot at job end" as the primary sharing mechanism**.
  Kiseki clients need continuous visibility of ongoing writes.
- **Manually-scheduled compaction runners**. Kiseki needs automatic,
  resource-bounded, continuous compaction.
- **Trust-based tenancy** (jobs trust each other implicitly because
  they're on the same cluster). Kiseki needs enforced tenancy.

---

## 3. Mochi — detailed comparison

### 3.1 What Mochi is

Mochi is not a storage system. It is a *framework* for composing HPC
data services from building blocks:
- **Mercury/Margo/Thallium** — RPC library over libfabric, with
  support for Slingshot (CXI provider), IB verbs, TCP, shared memory
- **Bake** — RDMA-accessible BLOB storage microservice
- **SDSKV** — key-value store microservice (with multiple backends)
- **SSG** — Scalable Service Groups, for group membership and
  bootstrap
- **REMI** — file migration microservice
- **Sonata** — JSON document store
- **Poesie** — embedded language interpreters (for service
  extensibility)

Services are composed by linking microservices into a process. Mochi
services have been used to build HEPnOS (high-energy physics data
store), Colza (elastic in-situ visualization), and DeltaFS itself.
DeltaFS uses Mercury for RPC.

### 3.2 Why Mochi is relevant to Kiseki

Kiseki's N×M pluggability commitment maps almost one-to-one onto
Mochi's methodology. Specifically:

- Kiseki's "transport plugin" is what Mercury is
- Kiseki's "chunk layer" is what Bake is
- Kiseki's "metadata KV backend" is what SDSKV is
- Kiseki's "service discovery" is what SSG is
- Kiseki's "gateway as stateful stream processor" maps to Mochi's
  provider/client composition

This means Kiseki does not have to build from zero. Mochi's libraries
are production-quality, actively maintained, Apache-licensed, and
already proven on Slingshot. Building Kiseki *on top of* Mochi is a
serious option.

### 3.3 The case for building on Mochi

- **Mercury is the right transport abstraction for what you want.** It
  supports Slingshot natively via libfabric/CXI, with TCP fallback.
  It has a mature async runtime (Argobots via Margo). It handles RDMA
  one-sided ops, eager vs. bulk transfers, and credential handling.
- **Bake solves the chunk-storage-node problem.** It exposes RDMA-
  addressable blob storage with a simple API. Multiple Bake
  providers can be used by the same composed service.
- **SDSKV gives you multiple KV backends plug-and-play** (LevelDB,
  BerkeleyDB, etc.). Kiseki's log-shard storage could use SDSKV
  directly.
- **You don't build consensus, RPC, discovery, or low-level
  transport.** You compose them.
- **Your code surface shrinks to the parts Kiseki is actually
  novel in**: log ordering semantics, composition-over-chunks,
  view materialization, multi-tenancy enforcement, protocol
  gateways.

### 3.4 The case against building on Mochi

- **C/C++ ecosystem.** Mochi is C (Mercury, Margo) and C++ (Thallium).
  Your language commitment is Rust. Interop is possible via FFI but
  adds friction. If Kiseki's core is Rust, every Mochi boundary
  requires marshaling.
- **Abstraction mismatch.** Mochi's abstractions are shaped by HPC
  batch-job assumptions (single trust domain, per-job lifecycle).
  Kiseki's persistent, multi-tenant model may fight Mochi's
  abstractions at the boundaries.
- **Dependency risk.** Mochi is academic/lab code. Active
  development, but smaller bus factor than a commercial project.
  You'd be betting on the Argonne MCS group's continued investment.
- **You lose control of critical path components.** Performance
  bugs in Mercury become your performance bugs, with a slower
  fix cycle than if you controlled them.

### 3.5 The honest assessment

Mochi is not the obvious right answer, but it is also not the
obvious wrong answer. The analyst should put this squarely on the
table as a question: *"Build Kiseki on top of Mochi's Mercury/Bake/
SDSKV substrate, or build the equivalents in Rust?"*

If building in pure Rust, the equivalents are:
- Mercury → a Rust libfabric binding (exists but immature, e.g.,
  `libfabric-sys`) plus a custom RPC layer, or tonic/gRPC (which
  doesn't natively use RDMA)
- Bake → roll your own, with something like `io_uring` plus a chunk
  manager
- SDSKV → embed RocksDB or Sled or Heed (all mature Rust options)
- SSG → use an existing service mesh library or raft membership
- Compaction → roll your own on top of the chosen LSM library

The Rust ecosystem has most of the pieces. The ones that are
weakest are around libfabric (the Slingshot-native binding) and
around mature RDMA primitives. These are exactly the parts Mochi
does well.

A pragmatic middle ground: use Mochi for the HPC-native data path
(Mercury/Bake for performance-critical client traffic over Slingshot)
and use Rust for the rest. This has the unsatisfying property of
straddling two language ecosystems, but it buys you production-
quality transport without committing to C++ for everything.

### 3.6 Mochi and encryption

Like DeltaFS, Mochi has no built-in encryption story. Mercury
supports TLS-wrapped transports for TCP, but the high-performance
RDMA paths (verbs, CXI) are not encrypted at the Mochi layer. Bake
stores blobs in whatever form is handed to it — it does not
encrypt. SDSKV likewise.

If Kiseki builds on Mochi, the encryption layer is above Mochi's
abstractions: clients/gateways encrypt before handing payloads to
Bake, and encrypt-then-MAC before handing log entries to SDSKV.
Mochi becomes a ciphertext-transport and ciphertext-store
substrate. This is architecturally clean but means Mochi's
performance-tuning tools have less visibility into the actual
work being done (they see opaque encrypted blobs). It also means
the encryption commitment is not weakened by Mochi — Mochi is
neutral on encryption.

---

## 4. Related systems briefly evaluated

### 4.1 DAOS (Intel, now Linux Foundation)

Architecturally the closest *product-quality* system to what Kiseki
is proposing. Open source (Apache 2.0), Slingshot-capable via
libfabric, POSIX and S3 interfaces, distributed-by-default metadata.

Post-Optane DAOS runs on regular NVMe — concerns raised during the
conversation about DAOS reliability and metadata wipes may or may not
still apply. The analyst should verify current DAOS stability before
committing to "DAOS is not an option."

DAOS's architectural differences from Kiseki:
- DAOS has a single persistent memory tier conceptually (even on
  regular NVMe post-Optane); Kiseki has explicit affinity-pool
  multi-tier
- DAOS multi-tenancy is container-based with ACLs; Kiseki's proposed
  tenancy is deeper (service placement per tenant)
- DAOS is written in C; Kiseki commits to Rust+Go
- DAOS is actively maintained with vendor investment; Kiseki would be
  a new project

### 4.2 CephFS with device classes

CRUSH device classes give you affinity-pool-like behavior. RGW gives
you S3. CephFS gives you POSIX. Multi-tenancy is reasonable. The
performance problem named in the conversation is real. Kiseki's
proposed architecture is fundamentally different from Ceph's RADOS
+ MDS model, so there's limited architectural overlap, but Ceph is
the relevant point of comparison for "off-the-shelf multi-protocol
storage."

### 4.3 Lustre on Slingshot

The current substrate being replaced. Worth noting that Lustre on
Slingshot actually performs well — the issues named (MDS bottleneck,
weak multi-tenancy, unreliability) are structural, not performance-
related. The gap Kiseki is filling is *not* "Lustre is slow" — it's
"Lustre is operationally painful and tenant-hostile."

---

## 5. What this evaluation does not settle

The analyst should probe these specifically:

1. **Is building on Mochi the right call?** Not addressed by the
   design conversation, and it's a major architectural decision.
2. **Has current DAOS been re-evaluated recently?** The "DAOS is
   unreliable" claim is foundational to the decision to build, and
   if it's stale, the project calculus changes dramatically.
3. **What does Kiseki guarantee that DeltaFS does not?** The
   differentiators (persistence, multi-tenancy, standard protocols)
   are named here. Whether they are actually the reasons to build
   this thing — rather than adopting DeltaFS + extensions — is a
   domain expert call.
4. **Which DeltaFS findings are show-stoppers?** The compaction
   tail-latency issue, the per-tenant throughput ceiling, the
   WAL-latency cost — these are problems that scale with
   workload and cluster size. Whether they're acceptable for
   Kiseki's target deployments has not been confirmed.
5. **What's the right v1 scope given this prior art?** "S3 over
   TCP on hyperconverged ClusterStor nodes using Mochi transports,
   single tenant to start, FoundationDB for metadata KV" is one
   concrete v1. Many others are possible. The conversation did not
   commit.

6. **How does first-class encryption interact with every substrate
   choice?** DeltaFS has no encryption. Mochi has no encryption.
   Rust ecosystem has good AEAD libraries (ring, RustCrypto) but
   no particular storage-encryption idiom. Whatever Kiseki builds
   on, encryption is Kiseki's to design. The analyst needs to
   confirm this is understood — it's not a feature to add later,
   it's a pillar that shapes the log format, the envelope, the
   wire protocol, and the key-management boundary.

---

## 6. Recommendations to the analyst

1. **Do not assume the design is settled.** The conversation converged
   on a coherent architecture, but it converged on an architecture that
   is so close to DeltaFS that the differentiation story needs
   interrogation.
2. **Interrogate the build-vs-adopt question early.** If DeltaFS or
   DAOS or Mochi-based composition can be extended to cover Kiseki's
   needs, a green-field rewrite in Rust is hard to justify.
3. **Treat "multi-tenancy, standard protocols, persistent service" as
   the pillars of the differentiation story.** These are the parts
   that genuinely need new architecture. The rest should reuse prior
   art where possible.
4. **Probe the persistence-vs-job-scope tension carefully.** It's the
   biggest structural difference from DeltaFS and it drives most of
   the "this is harder than DeltaFS" consequences.
5. **Put the Mochi question on the table explicitly.** Build-on-Mochi
   vs. build-in-Rust is a decision that shapes language boundaries,
   dependency risk, and scope. It deserves an ADR.

6. **Treat the encryption design as Kiseki-original.** Neither
   DeltaFS nor Mochi provide an encryption substrate to inherit.
   The analyst should schedule a dedicated session (Session 4b in
   SEED.md) for crypto interrogation and should not allow
   downstream specs to assume encryption is "solved" by any
   substrate choice. Threat model, key hierarchy, KMS boundary,
   and crypto-shred contract are all Kiseki's to specify.
