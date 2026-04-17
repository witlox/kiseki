# Analyst Seed — Kiseki

**Read this after** `docs/analysis/design-conversation.md` and
`docs/prior-art/deltafs-mochi-evaluation.md`.

**This is NOT a spec.** It is pre-interrogation context: candidate
terms, suspected invariants, and the question bank the analyst should
probe first. The analyst's job is to interrogate the domain expert
(the user) and produce the actual specs in `specs/`.

Per the analyst role definition: do not generate specs without
interrogation; do not defer to the domain expert; ask no more than
3 questions at a time; state inferences explicitly.

---

## 1. Candidate ubiquitous language (UNVALIDATED)

These are terms the design conversation used with apparent consistency.
Every one needs confirmation with the domain expert that the term means
one thing and only one thing.

| Candidate term | Working definition | Probe |
|---|---|---|
| Delta | A single metadata mutation recorded in the log | Is a delta always metadata, or also data? |
| Chunk | Opaque content-addressable or UUID-keyed data segment | Fixed-size or variable-size? Is chunk identity content-derived? |
| Composition | Metadata structure describing how to assemble chunks into a data unit | Is a composition itself a chunk? Is it stored in the log or somewhere else? |
| Log | Ordered, replicated sequence of deltas | Is "log" always scoped to a shard, or is there a global log concept? |
| Log shard / Consistency set | The smallest unit of totally-ordered deltas | Are "log shard" and "consistency set" the same thing? |
| View | Protocol-shaped materialized projection of the log | Does a view include data, or only metadata? Can a view span multiple compositions? |
| View descriptor | Declarative specification of a view's shape, tier, semantics | What's in a descriptor? |
| Tenant | Isolation boundary for namespaces, services, keys, accounting | Is a tenant a hierarchical concept (sub-tenants)? |
| Namespace | Tenant-scoped collection of compositions | Is namespace coextensive with "consistency set" or independent? |
| Affinity pool | Group of nodes with a given device class | Can a node belong to multiple pools? |
| Flavor | Named (protocol, transport, topology, access-path) deployment configuration | Is flavor per-cluster or per-tenant? |
| Native client | Userspace client library with FUSE and native-library interfaces | Is "native client" always client-side, or can server-side code use the same library? |
| Protocol gateway | Server-side component translating (wire protocol, transport) to log/view operations | Is a gateway always tenant-scoped, or can it be shared? |
| Stream processor | Component that consumes the log and maintains a view | Same as gateway, or separate? |
| Tenant key | Cryptographic key material controlled by a tenant, required to decrypt that tenant's data | Is there one key per tenant, or a hierarchy (tenant master → namespace → chunk)? |
| Data encryption key (DEK) | Symmetric key that directly encrypts chunk or log payloads | Per-chunk? Per-composition? Per-shard? |
| Key encryption key (KEK) | Key that wraps DEKs (envelope encryption) | Is this the tenant-controlled boundary? |
| Crypto-shred | Deletion performed by destroying the key, rendering ciphertext unreadable | Is this the only deletion, or is there also physical chunk GC? |
| Key management service (KMS) | Subsystem holding, rotating, escrowing, and issuing keys | Internal to Kiseki, external dependency, or pluggable? |
| Envelope | The wrapped structure containing ciphertext + authenticated metadata + wrapped key reference | What's in the envelope; what's authenticated? |

**Synonym watch**: these pairs might be the same thing:
- "log shard" and "consistency set"
- "gateway" and "stream processor"
- "view" and "materialization"
- "namespace" and "tenant namespace"

Each pair needs: are these the same, or different? If different, what
distinguishes them?

---

## 2. Candidate bounded contexts

Before the analyst can produce a domain model, bounded contexts need
explicit confirmation. The conversation implied roughly six:

1. **Log** — accepts deltas, orders them, replicates them, persists
   them, supports range reads. This is the consensus/durability layer.
2. **Chunk storage** — stores and retrieves opaque data chunks, handles
   placement across affinity pools, manages replication/EC, runs GC.
3. **Composition / View materialization** — consumes deltas, maintains
   materialized views per descriptor, handles view lifecycle (create,
   update, tier, discard, rebuild).
4. **Protocol gateway** — translates wire protocols (NFS, S3) into
   log/view operations; runs as stateful stream processors.
5. **Native client** — runs in workload processes; detects access
   patterns, selects views, uses best transport, exposes POSIX (FUSE)
   and native API.
6. **Control plane** — declarative API, tenancy, policy, placement
   decisions, flavor configuration, auto-discovery outputs.
7. **Key management** — custody, rotation, escrow, and issuance of
   tenant keys; wrapping and unwrapping operations; crypto-shred
   orchestration. Surfaced late in the design conversation; may be
   folded into the control plane or stand alone. Because it gates
   every read and write, its availability and integrity properties
   are as critical as the log's.

**Probes**:
- Is "view materialization" separate from "protocol gateway" or are
  they one context?
- Does the native client have its own context or does it belong to
  "protocol gateway" from the client's perspective?
- Is "authorization / key management" a seventh context or is it
  embedded in the control plane?
- If key management is its own context, where is the boundary
  between "tenant owns the keys" and "system orchestrates
  encryption operations"?
- Does key management own both symmetric data keys (DEKs) and
  wrapping keys (KEKs), or just the wrapping layer?
- Is "observability / telemetry" a context at all, or a concern
  across all contexts?

---

## 3. Suspected invariants (UNVALIDATED)

These are properties that *might* have to hold. Each needs confirmation.
Some are clearly load-bearing; some may be wrong. The analyst should
treat each as a hypothesis.

### 3.1 Log invariants

- I-L1: *Within a log shard, deltas have a total order.*
- I-L2: *A committed delta is durable on a majority of replicas in its
  Raft group before ack.*
- I-L3: *A delta is immutable once committed.*
- I-L4: *Garbage collection of deltas requires that all views consuming
  from the shard have advanced past the delta's position.*
- I-L5: *A delta references only chunks that exist (or are being
  written concurrently in a protocol-defined order).*

### 3.2 Chunk invariants

- I-C1: *Chunks are immutable. New versions are new chunks.*
- I-C2: *A chunk is not garbage-collected while any composition
  references it.* (→ refcount mechanism required)
- I-C3: *Chunks are placed according to their affinity policy, which
  is derived from the referencing composition's view descriptor.*

### 3.3 Composition invariants

- I-X1: *A composition belongs to exactly one tenant.*
- I-X2: *A composition's chunks respect the tenant's dedup policy
  (intra-tenant vs. cross-tenant).*
- I-X3: *A composition's mutation history is fully reconstructible
  from its log shard's deltas.*

### 3.4 View invariants

- I-V1: *A view is derivable from its source log shard(s) alone — no
  external state is required.* (This is the "rebuildable from log"
  property.)
- I-V2: *A view's observed state is a consistent prefix of its source
  log(s) up to some watermark.*
- I-V3: *Reads from a view see a snapshot at a specific log position
  (MVCC).*

### 3.5 Tenant invariants

- I-T1: *Tenants cannot read each other's compositions without
  explicit cross-tenant authorization.*
- I-T2: *A tenant's resource consumption (capacity, IOPS,
  metadata ops) is bounded by its quotas.*
- I-T3: *A tenant's keys are not accessible to other tenants or to
  shared system processes.*

### 3.6 Encryption / key invariants

- I-K1: *No plaintext chunk is ever persisted to storage.*
- I-K2: *No plaintext payload is ever sent on the wire.* (Authenticated
  metadata envelopes may be visible, but payload is always encrypted.)
- I-K3: *Log deltas containing tenant metadata are encrypted with
  tenant keys before being committed to the Raft log.*
- I-K4: *The system can enforce access to ciphertext without being
  able to read plaintext without tenant key material.* (This is the
  "tenant controls their keys" property, stated as an invariant.)
- I-K5: *Crypto-shred (key destruction) renders previously-accessible
  ciphertext unreadable across all replicas, caches, and backups
  within a bounded time window.* (Bounded how? That's a question.)
- I-K6: *Key rotation does not lose access to data encrypted under
  prior keys until an explicit cutover.* (i.e., rotation is not a
  destructive operation by default.)
- I-K7: *Authenticated encryption is used everywhere; unauthenticated
  encryption is never acceptable.*
- I-K8: *Keys are never logged, never printed, never transmitted in
  the clear, never stored in configuration files.*

### 3.7 Suspected-but-unclear invariants

- I-?1: *Cross-view read-your-writes within a single tenant.*
  (Does a writer on the NFS view see their own write when reading the
  S3 view? The conversation didn't settle this.)
- I-?2: *Cross-shard operation atomicity.* (2PC was named; guarantees
  were not specified.)
- I-?3: *Native-client cache coherence across clients.* (If two
  clients use the native client, does A see B's write? When?)

---

## 4. Failure-mode candidates (for Layer 5)

The conversation surfaced these failure modes explicitly or
implicitly. Each needs a blast radius, a detection mechanism, and a
desired degradation strategy.

- **Raft leader loss for a log shard** — quorum survives, new leader
  elected, transient latency bump. Blast radius: the shard. Detection:
  Raft heartbeat. Degradation: wait for election.
- **Raft quorum loss** — shard becomes unavailable. Blast radius: all
  compositions in that consistency set. Detection: ack timeout.
  Degradation: reads may continue from replicas if stale reads are
  permitted; writes fail.
- **Log corruption** — a shard's log cannot be replayed. Blast radius:
  catastrophic for the consistency set. Detection: WAL checksum on
  replay, materialization consistency check. Degradation: ??? (The
  conversation did not specify. This is a serious gap.)
- **Chunk loss (EC too lossy, disk failures)** — data corruption.
  Blast radius: compositions referencing the lost chunk. Detection:
  erasure coding verification. Degradation: repair from replicas,
  rebuild from parity, or data loss acknowledged.
- **Protocol gateway crash** — affected tenant's clients disconnect.
  Blast radius: one tenant's access via that protocol. Detection:
  liveness probe. Degradation: restart gateway; clients reconnect.
- **View materializer fell behind** — clients see stale data through
  that view. Blast radius: one view. Detection: watermark lag
  threshold. Degradation: redirect clients to a fresher view if
  available; alert operator.
- **Native client crash on a compute node** — that client's
  in-flight operations fail. Blast radius: one client process.
  Detection: TCP/RDMA connection loss. Degradation: client retries;
  uncommitted writes are lost (same as any client).
- **Network partition** — Raft handles it for the log; but views on
  one side may diverge from views on the other if they consume from
  different replicas with stale reads. Blast radius: affected tenants.
  Detection: partition detection. Degradation: ???
- **Compaction storm** — background compaction can't keep up with
  foreground writes. Blast radius: shard-local. Detection: SSTable
  count per shard. Degradation: back-pressure on writes? Throttle?
  Allocate more compaction compute?
- **Topology change (node added/removed)** — rebalancing required.
  Blast radius: placement changes across cluster. Detection: topology
  service. Degradation: gradual rebalance with throttling.
- **KMS unavailability** — no new encrypt/decrypt operations can
  proceed without keys. Blast radius: all tenants whose keys are
  held in the unavailable KMS. Detection: KMS liveness probe.
  Degradation: cache recent keys client-side for a bounded window?
  Fail reads/writes that need fresh key material? This is a real
  availability question — the KMS becomes a critical-path
  dependency.
- **Tenant key loss (catastrophic)** — the tenant cannot decrypt
  their own data. Blast radius: that tenant's data is effectively
  gone. Detection: decrypt failure. Degradation: none if no escrow.
  If escrow exists: recovery via escrow process (which violates
  the pure "tenant controls keys" property).
- **Key compromise** — an attacker holds a copy of a tenant key.
  Blast radius: all data encrypted under that key. Detection:
  out-of-band (audit, threat intel). Mitigation: rotate key,
  re-encrypt data (or re-wrap with new KEK, depending on
  envelope structure). This is an incident-response workflow as
  much as a technical failure mode.
- **Crypto-shred incomplete** — key destruction succeeded in
  primary KMS but a cached copy survives somewhere (client
  memory, backup, replica). Blast radius: data is not actually
  deleted even though the tenant believes it is. Detection:
  essentially undetectable — this is a trust-the-process
  concern. Mitigation: bounded cache lifetimes, explicit
  invalidation propagation, audit logs of key-material lifecycle.
- **Replay attack on encrypted logs** — an attacker replays
  captured encrypted deltas. Blast radius: depends on whether
  the log's authenticated-encryption scheme binds to position /
  nonce / sequence. Detection: sequence-number enforcement.
  Mitigation: AEAD with monotonic nonces tied to log position.
- **Algorithm deprecation / crypto-agility** — an encryption
  algorithm in use becomes considered unsafe. Blast radius: all
  data encrypted with it. Detection: external (NIST, CVE).
  Mitigation: need a designed migration path — the envelope
  format should carry algorithm identifiers and the system
  should support multiple algorithms concurrently during
  migration.

---

## 5. First-round question bank

The analyst should prioritize these. Not all at once — the role spec
says no more than 3 at a time.

### 5.1 Existential / framing questions

- **Q-E1**: Given DeltaFS exists and is remarkably close architecturally,
  what does Kiseki do that DeltaFS + extensions could not? The
  differentiators named in prior art (persistence, multi-tenancy,
  standard protocols, first-class encryption) — are these the real
  reasons to build, or is there a deeper reason?
- **Q-E2**: Is Kiseki expected to be production-grade from the start,
  a research prototype, or somewhere in between? The answer changes
  almost every downstream decision about reliability, observability,
  and scope.
- **Q-E3**: Is building on Mochi's Mercury/Bake/SDSKV on the table, or
  ruled out? The decision shapes language boundaries and dependency
  risk substantially.
- **Q-E4**: What is the threat model? "First-class encryption" means
  little until the adversary is named. Honest-but-curious operator?
  Malicious insider? Physical drive theft? Network observer? State
  adversary? Different answers produce different requirements for
  key custody, metadata confidentiality, side-channel resistance,
  and algorithm choices. This is the threat model question from
  Q-K12 promoted to existential because the whole encryption design
  depends on it.

### 5.2 Scope-bounding questions

- **Q-S1**: What does Kiseki explicitly refuse to do? "Small-random-
  write POSIX workloads" was floated as a non-goal. Confirmed?
- **Q-S2**: What is v1 scope? One protocol, one transport, one
  topology — or broader?
- **Q-S3**: Is multi-site (geo-distributed) in scope at all, or is
  this strictly single-site?

### 5.3 Consistency model questions

- **Q-C1**: What consistency does a client get when reading data they
  just wrote, over the same protocol?
- **Q-C2**: What consistency does a client get when reading over a
  different protocol than they wrote with?
- **Q-C3**: What consistency does a client get when reading across
  tenants (if cross-tenant authorization exists)?
- **Q-C4**: Under partition, does the system prefer availability or
  consistency? (CAP positioning — probably CP for metadata, AP for
  some read paths, but needs confirmation.)

### 5.4 Tenancy questions

- **Q-T1**: What does a tenant mean? A person? A project? An
  organization? A workload?
- **Q-T2**: Is there a tenant hierarchy (sub-tenants, departments
  within orgs)?
- **Q-T3**: What resources does a tenant control? (Namespaces,
  keys, quotas, gateway instances, dedup scope, encryption
  settings, audit logs?)
- **Q-T4**: Is tenant isolation best-effort or enforced? If enforced,
  at what layer — protocol gateway, log access, chunk access?

### 5.5 Data lifecycle questions

- **Q-D1**: How is data deleted? Crypto-shred (delete key, data
  becomes unreadable)? Actual chunk deletion? Tombstone in log then
  compact? Time-delay?
- **Q-D2**: What are the retention policies? Are there regulatory
  requirements (GDPR right-to-be-forgotten, HIPAA, export control)
  that affect the design?
- **Q-D3**: How are snapshots, backups, or versioning surfaced
  (if at all) in v1?

### 5.6 Operational questions

- **Q-O1**: Who operates a Kiseki cluster? An HPC admin team?
  A cloud ops team? The tenant themselves?
- **Q-O2**: What's the upgrade story? Rolling upgrades across
  heterogeneous versions? Coordinated full-cluster upgrades?
- **Q-O3**: What's the observability contract? What metrics,
  traces, logs must Kiseki emit?
- **Q-O4**: What's the backup story? Is the log itself the backup
  (replicated, durable), or does Kiseki support external backup?

### 5.7 Failure-semantics questions

- **Q-F1**: If a log shard is corrupted beyond Raft recovery, what
  is the desired behavior? Data loss acknowledged? Rebuild from
  backup? Human intervention?
- **Q-F2**: What is the maximum tolerable write latency at p99?
  At p99.9?
- **Q-F3**: What is the maximum tolerable read latency at the same
  percentiles?
- **Q-F4**: What fraction of nodes can be lost before the system
  refuses to make progress?

### 5.8 Encryption and key-management questions

All of these need to be answered before specs in the key-management
context (Layer 3 onward) can be written.

- **Q-K1**: Where does encryption happen in the data path? Native
  client encrypts before it leaves the workload? Protocol gateway
  encrypts on ingress? Storage node encrypts at rest only? The
  answer shapes who holds plaintext and for how long.
- **Q-K2**: What is the key hierarchy? Flat per-tenant keys?
  Envelope encryption with per-chunk DEKs wrapped by per-tenant
  KEKs? Additional layers (per-namespace, per-composition)?
- **Q-K3**: Who holds the KEKs? External customer KMS (AWS KMS,
  HashiCorp Vault, HSM)? Internal Kiseki KMS? Both, at tenant's
  choice? This determines the "tenant controls their keys"
  property's strictness.
- **Q-K4**: Is key escrow acceptable? An escrow/recovery mechanism
  means someone other than the tenant can recover keys — violating
  pure tenant control. Some compliance regimes require this; some
  forbid it. Domain expert must choose.
- **Q-K5**: What are the key-rotation semantics? On rotation, is
  existing ciphertext re-encrypted (expensive, long-running,
  transactional) or only the wrapping key updated (cheap, but
  compromise of old DEK still decrypts old ciphertext)?
- **Q-K6**: What crypto primitives are mandatory? AES-GCM-256 is
  the safe default. AES-GCM-SIV is better for nonce-misuse
  resistance. ChaCha20-Poly1305 is faster without AES-NI. Is
  FIPS 140-3 compliance required? Post-quantum readiness?
- **Q-K7**: How does encryption interact with cross-tenant dedup?
  "No cross-tenant dedup, ever" is a simple and defensible
  position that the conversation implicitly leaned toward. Needs
  explicit confirmation — if the domain expert wants global
  dedup back, the crypto design changes substantially.
- **Q-K8**: What is the wire-encryption strategy for Slingshot
  one-sided RDMA? CPU-based AES-NI? Cassini NIC offload (if
  supported)? Client-side decryption of pre-encrypted blocks
  transferred via one-sided reads? The choice determines whether
  RDMA performance advantages survive the encryption commitment.
- **Q-K9**: What does crypto-shred guarantee, and within what
  time window? "Eventually unreadable" is weak. "Unreadable
  within 60 seconds across all replicas, caches, and backups" is
  strong and operationally meaningful. This is a contract question.
- **Q-K10**: What is the audit requirement? Key lifecycle events
  (creation, rotation, escrow access, destruction) need to be
  auditable. Data access events may also need audit. What's the
  contract, and where is the audit log stored (it itself needs
  integrity)?
- **Q-K11**: Is metadata confidentiality (filenames, sizes,
  directory structure hidden from non-tenant observers) required,
  or only data confidentiality? The former is significantly
  harder to achieve and verify.
- **Q-K12**: What threat model is Kiseki defending against?
  Honest-but-curious operator? Malicious insider? Physical drive
  theft? Network observer? State-level adversary? The answer
  drives which of the above questions have stronger or weaker
  answers.

### 5.9 Anti-patterns the analyst should watch for

- Conflating "log" with "WAL" — they serve different purposes in
  this architecture
- Conflating "view" with "cache" — views are consistency-first,
  caches are performance-first
- Conflating "shard" with "tenant" — a tenant can span shards; a
  shard can host multiple tenants (or not — this needs
  confirmation)
- Treating "native client" as mandatory — it's the fast path, not
  the only path
- Treating Slingshot as required — it's the target high-performance
  transport, not the only supported one
- Treating encryption as "we'll add it later" — the domain expert has
  declared it first-class, and retrofitting encryption into a
  data-path and metadata layout is historically one of the hardest
  migrations in storage systems
- Assuming the threat model defaults to "honest operator" — the
  "tenant controls their keys" commitment implies a stronger threat
  model (at least "curious operator"), which the analyst should
  make explicit
- Conflating "encrypted at rest" with "encrypted in flight" with
  "encrypted in the log" — these are three distinct architectural
  commitments with different implementation paths

---

## 6. Assumptions log — pre-seeded

The analyst should take these as starting assumptions and continue
maintaining them. Per the role definition: validated / accepted with
acknowledged risk / unknown.

| # | Assumption | Status | Risk if false |
|---|---|---|---|
| A1 | Workloads are dominated by large sequential reads, bulk writes, and object-access patterns | Unknown | If false, architecture is wrong for the target workload |
| A2 | Current DAOS still has reliability problems | Unknown | If false, build-vs-adopt calculus changes |
| A3 | Mochi on Slingshot is production-ready | Unknown | If false, Mochi option closes |
| A4 | ClusterStor E1000/E1000F hardware is sufficient for the envisioned tenant count | Unknown | If false, scale targets are wrong |
| A5 | Tenants will tolerate bounded-staleness cross-protocol reads | Unknown | If false, consistency story needs rework |
| A6 | Raft-per-shard is operationally acceptable | Accepted (risk) | If the shard count grows into thousands, Raft overhead may dominate |
| A7 | FUSE overhead is acceptable for the POSIX path | Accepted (risk) | If workloads are latency-sensitive on POSIX, FUSE may be too slow; kernel module may be needed |
| A8 | Reactive tiering within declarative bounds is stable | Unknown | If false, auto-tiering failure modes (thrashing) could recur |
| A9 | Slingshot CXI provider in libfabric is mature enough for production use | Unknown | If false, transport strategy is wrong |
| A10 | Rust async ecosystem (tokio) supports storage-system workload patterns | Accepted | If async contention becomes a bottleneck, may need to use blocking threads for some paths |
| A11 | Cross-tenant dedup is out of scope (intra-tenant only) | Unknown | If domain expert wants global dedup, the crypto design changes substantially and convergent-encryption side-channels become a concern |
| A12 | FIPS 140-3 compliance is not a hard requirement | Unknown | If required, algorithm choices are narrowed and module validation becomes a deliverable |
| A13 | Post-quantum crypto readiness is future work, not v1 | Accepted (risk) | If an agency/regulatory requirement lands, envelope format must already support algorithm identifiers to allow PQ migration |
| A14 | NIC-offloaded wire encryption on Slingshot Cassini is not production-ready as of the design conversation | Unknown | If false, one-sided RDMA with encryption becomes significantly more viable; if true, either CPU-encrypt or design around it |
| A15 | Tenants operate their own external KMS (or are willing to) | Unknown | If tenants expect Kiseki to manage all keys internally, the KMS context becomes much larger in scope and the "tenant controls keys" property weakens |
| A16 | The threat model is at minimum "curious operator" (operator is not assumed trustworthy with plaintext) | Unknown | If threat model is weaker (honest operator), encryption can be simpler; if stronger (active adversary), some design choices above are insufficient |

---

## 7. What the analyst should NOT do

Per the role definition:
- Do not write code
- Do not make architectural decisions (that's the architect)
- Do not generate specs without interrogation

Additionally, specific to this project:
- Do not assume the design conversation settled things it actually
  didn't. The conversation produced commitments and also produced
  tensions; both are present in `design-conversation.md`.
- Do not collapse "Kiseki differs from DeltaFS" into "Kiseki is
  better than DeltaFS." They serve different use cases. The
  differentiators are specific.
- Do not commit to v1 scope without the domain expert confirming it.
  The conversation suggested several possible v1s.
- Do not treat the Mochi question as settled. It isn't.

---

## 8. Recommended analyst session plan

Session 1 — framing and existential questions:
  Ask Q-E1, Q-E2, Q-E3, Q-E4. Based on answers, confirm/revise the
  existence of the project, its target deployment profile, and its
  threat model. The threat model (Q-E4) is deliberately elevated
  from the encryption-specific question block because the answer
  cascades into tenancy, observability, key management, and
  potentially hardware choices.

Session 2 — domain model (Layer 1):
  Confirm bounded contexts (§2). Walk through candidate terms (§1)
  and pin down ubiquitous language. Eliminate synonyms. Produce
  `specs/ubiquitous-language.md` and first cut of
  `specs/domain-model.md`.

Session 3 — invariants (Layer 2):
  Walk through suspected invariants (§3) one bounded context at a
  time. Confirm, revise, or reject each. Surface new invariants the
  expert proposes. Produce `specs/invariants.md`.

Session 4 — tenancy deep-dive (probably needs its own session):
  Ask Q-T1 through Q-T4. Resolve density vs. isolation trade-off.
  Resolve cross-tenant authorization model. This shapes much of
  the later behavioral spec.

Session 4b — encryption and key-management deep-dive (its own
session):
  Ask Q-K1 through Q-K12. Establish the threat model explicitly.
  Decide key hierarchy, KMS boundary, escrow policy, algorithm
  choices, crypto-shred contract. Decide whether key management
  is its own bounded context (likely yes). This is likely a long
  session — encryption choices cascade through every other context,
  so the decisions made here become constraints for Sessions 5–8.

Session 5 — behavioral specification (Layer 3):
  Per bounded context, produce Gherkin scenarios for happy paths,
  failure paths, edge cases. Expect this to take multiple sessions.

Session 6 — cross-context interactions (Layer 4):
  Map which contexts talk to which. Define contracts.

Session 7 — failure modes (Layer 5):
  Walk through §4. Add, revise, refine. Blast radius + detection +
  degradation for each.

Session 8 — assumptions review (Layer 6):
  Walk through §6. Re-classify based on what interrogation has
  revealed. Flag any that would invalidate the architecture.

Session 9 — final adversarial pass:
  Before graduation to architect, do a deliberate completeness
  check: what questions have we not asked? What bounded contexts
  haven't been fully explored? What invariants are asserted but
  not tested?
