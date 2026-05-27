# ADR-044: Convergent encryption for content-addressed dedup

**Status:** Accepted (adversary-reviewed 2026-05-27; findings addressed below)
**Date:** 2026-05-27
**Relates to:** ADR-002 (two-layer encryption), ADR-003 (HKDF DEK derivation), ADR-017 (dedup + refcount), GH #102

## Context

Kiseki deduplicates chunks by content address: `chunk_id = HMAC-SHA256(plaintext)`
(`DedupPolicy::CrossTenant`) or `HMAC-SHA256(tenant_hmac_key, plaintext)`
(`DedupPolicy::TenantIsolated`) — `crates/kiseki-crypto/src/chunk_id.rs`. Two
writes of identical content therefore produce the **same `chunk_id`**, which is
the intended dedup signal (refcount instead of re-store, ADR-017).

Chunks are encrypted with AES-256-GCM. The DEK is derived per chunk:
`dek = HKDF-SHA256(master[epoch], salt=chunk_id, info="kiseki-chunk-dek-v1")`
(ADR-003). But `Aead::seal` generates a **random** GCM nonce per call. So two
writes of identical content produce the **same `chunk_id` and DEK** but
**different ciphertext + nonce + auth_tag**.

On a single-node store this is harmless (the whole envelope — ciphertext +
nonce + tag — is stored together, last-write-wins atomically). On an
**erasure-coded cluster** it is not: the chunk's data is striped into N
fragments fanned out to N nodes, and the chunk-level crypto (nonce/tag) is
recorded in a per-node side registry — all **non-atomically**. Repeated or
concurrent writes of the same `chunk_id` overwrite different nodes' fragments
and the registry from **different write generations**. A subsequent read
reassembles a **torn** chunk: one generation's nonce/tag against another
generation's EC bytes → `AEAD authentication failed` (GH #102, reproduced on a
6-node EC-4+2 GCP cluster — every write/read carried one `chunk_id` with
mismatched nonce/tag/bytes).

Root cause: **content-addressed dedup is incompatible with non-deterministic
(random-nonce) encryption.** Dedup asserts "same content ⇒ same stored chunk";
random-nonce encryption makes "same content ⇒ different stored bytes."

## Decision

Adopt **convergent encryption**: derive the GCM nonce **deterministically** from
the chunk identity instead of at random.

```text
nonce = HKDF-SHA256(master[epoch], salt=chunk_id, info="kiseki-chunk-nonce-v1")[..12]
```

`seal_envelope` derives both the DEK and the nonce from `(master, chunk_id)` with
**distinct, versioned `info` labels** (domain separation). Identical content ⇒
identical `chunk_id` ⇒ identical DEK **and** nonce ⇒ **identical ciphertext +
auth_tag**. Writes of the same chunk are now idempotent: every fragment and the
registry crypto are byte-identical regardless of which write "wins," so the EC
reassembly can never tear.

`Aead` gains `seal_with_nonce(key, nonce, plaintext, aad)`; `seal` (random nonce)
is retained for the non-content-addressed callers — `wrap_for_tenant` and the
key-manager at-rest store (`kiseki-keymanager`), neither of which is
content-addressed or deduplicated, so a random nonce is correct there. The
nonce is still stored in the envelope and `open` still reads it from there — so
an envelope sealed with a random nonce would still decrypt (a free property of
the stored-nonce design, not a backward-compat constraint we engineered for —
Kiseki is pre-production with no durable data; see Consequences). Only new seals
become deterministic.

## Why this is GCM-safe

AES-GCM is catastrophic under (key, nonce) reuse across **distinct** plaintexts.
That never happens here: the DEK is `HKDF(master, salt=chunk_id, …)`, i.e.
**unique per `chunk_id`**, and `chunk_id` is a collision-resistant hash of the
content. So each DEK encrypts exactly one plaintext (the content that hashes to
its `chunk_id`), under exactly one deterministic nonce. The only "reuse" is
identical content → identical (key, nonce, plaintext) → identical ciphertext,
which is precisely the convergent property we want. Distinct content → distinct
`chunk_id` → distinct DEK (and nonce), so no key is ever reused across
plaintexts.

## Consequences

- **Fixes GH #102**: dedup workloads (and the benchmark's fixed payload) on EC
  clusters round-trip correctly.
- **Confirmation-of-content exposure** (the standard convergent-encryption
  trade-off): an adversary who can supply a candidate plaintext and observe
  either the dedup outcome (refcount vs new store) or the ciphertext can confirm
  whether that exact content already exists. This exposure already existed at the
  **dedup** layer (ADR-017: a dedup hit is observable); convergent encryption
  does not materially widen it beyond what content-addressing already implies.
  Bounding it is a **requirement, not a default** (adversary Finding 4):
  - Tenant data **MUST** use `DedupPolicy::TenantIsolated`
    (`chunk_id = HMAC(tenant_hmac_key, plaintext)`), which confines confirmation
    **within a single tenant** — an attacker must already hold that tenant's HMAC
    key.
  - `DedupPolicy::CrossTenant` (no tenant key) enables cross-tenant dedup and a
    cross-tenant confirmation oracle; it is **reserved for non-sensitive / system
    data only** and must never be selected for tenant payloads.
  - **Wiring (done):** `kiseki-server`'s data-plane gateway selects
    `TenantIsolated`, with the key from
    `kiseki_crypto::hkdf::derive_tenant_dedup_key(master, tenant_id)`
    (HKDF, info `kiseki-tenant-dedup-hmac-v1`). The `MemGateway`
    constructor default stays `CrossTenant` for system / non-tenant data;
    the gateway holds the key in `Zeroizing`. New tenant orgs default to
    `TenantIsolated`.
- **Two open constraints on the dedup key (adversary review of the wiring,
  2026-05-27):**
  - **Rotation stability (Finding B — tracked in GH #110).** Content-addressed dedup needs the
    dedup key to be **stable for the life of the tenant's data** — if it
    changes, identical content re-derives a new `chunk_id`, splitting
    refcount identity and breaking dedup continuity. `derive_tenant_dedup_key`
    currently keys on the **system master key**, which rotates (ADR-003 /
    `MasterKeyCache` epochs). This is **latent today** — the master key is a
    fixed constant and each gateway holds a single key, so no rotation
    occurs — but when real key management + rotation lands, the dedup key
    MUST hang off a **rotation-stable tenant root**, not the rotating system
    master. This lifetime requirement is a key-management design constraint
    (belongs alongside ADR-003); recorded here so it is not lost.
  - **Master-key sourcing (Finding C — tracked in GH #109).** The oracle-closure property (a
    *secret* `chunk_id` an attacker cannot compute offline) holds only once
    the system master key comes from the keymanager/KMS. Today it is the
    fixed placeholder `[0x42; 32]` (`runtime.rs`) used by the **entire**
    data-plane encryption, so the oracle is effectively open in any deploy
    of the current code. Acceptable for **pre-production** (no durable
    tenant data); it is **not** dev-gated — it is the only path — so the
    real master key MUST be sourced from the keymanager before any
    production deploy. This is a pre-existing data-plane-key gap, not
    specific to dedup.
- **Nonce secrecy is irrelevant** — GCM nonces are public (stored in the
  envelope). Determinism + per-key uniqueness are the only requirements, both
  met.
- **Crypto-agility**: the `…-nonce-v1` info label lets us rotate the derivation
  without ambiguity; the stored-nonce-on-open path means old chunks are never
  re-derived.
- **Migration / rollout (adversary Findings 1 & 2 — N/A pre-production):**
  Kiseki has no deployed clients yet (no durable data, no live fleet), so the
  forward-only nature of the fix is moot: a fresh deploy seals **every** chunk
  convergently from the first write, and any pre-existing test data is
  disposable. The properties only matter at GA and are recorded here to revisit
  then: (1) chunks torn by the old random-nonce path are not auto-repaired —
  they need delete+rewrite or a scrub pass; (2) a rolling upgrade across mixed
  old/new nodes could tear a chunk if both versions seal the *same new content*
  concurrently — so the GA cutover should be fleet-wide-atomic or gated. Neither
  applies now; do **not** add backward-compat machinery for them pre-GA.

## Alternatives considered

- **Random nonce + write-once dedup** (skip the store if `chunk_id` exists):
  still races on concurrent first-writes (two writers both see "absent" and
  store conflicting ciphertexts) → torn state persists.
- **Disable content-addressed dedup** (unique `chunk_id` per write): abandons
  the dedup feature (ADR-017) entirely.
- **Encrypt-then-hash** (`chunk_id = HMAC(ciphertext)`): breaks dedup (random
  nonce → different ciphertext → different id for identical content).

Convergent encryption is the standard resolution and the minimal change.
