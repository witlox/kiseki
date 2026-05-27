# ADR-044: Convergent encryption for content-addressed dedup

**Status:** Proposed (needs adversary review — security-sensitive)
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
is retained for any non-content-addressed callers. The nonce is still stored in
the envelope and `open` still reads it from there — so **existing chunks sealed
with a random nonce continue to decrypt unchanged; no migration**. Only new
seals become deterministic.

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
  whether that exact content already exists. Scope is bounded by the dedup
  policy: `TenantIsolated` (default for tenant data) confines it **within a
  tenant** (the attacker must already hold the tenant HMAC key). `CrossTenant`
  widens it cross-tenant and should be reserved for non-sensitive/system data.
  This exposure already existed at the **dedup** layer (ADR-017: a dedup hit is
  observable); convergent encryption does not materially widen it beyond what
  content-addressing already implies.
- **Nonce secrecy is irrelevant** — GCM nonces are public (stored in the
  envelope). Determinism + per-key uniqueness are the only requirements, both
  met.
- **Crypto-agility**: the `…-nonce-v1` info label lets us rotate the derivation
  without ambiguity; the stored-nonce-on-open path means old chunks are never
  re-derived.

## Alternatives considered

- **Random nonce + write-once dedup** (skip the store if `chunk_id` exists):
  still races on concurrent first-writes (two writers both see "absent" and
  store conflicting ciphertexts) → torn state persists.
- **Disable content-addressed dedup** (unique `chunk_id` per write): abandons
  the dedup feature (ADR-017) entirely.
- **Encrypt-then-hash** (`chunk_id = HMAC(ciphertext)`): breaks dedup (random
  nonce → different ciphertext → different id for identical content).

Convergent encryption is the standard resolution and the minimal change.
