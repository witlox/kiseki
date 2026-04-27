# Wire samples — Layer 1 cross-implementation seeds

Per ADR-023 §D2.3, every Layer 1 RFC test that asserts an encoded
shape SHOULD be anchored to a verbatim wire sample from a known-good
external implementation. This directory holds those samples.

## Directory layout

```
tests/wire-samples/
├── .gitattributes              # *.bin / *.pcap → Git LFS
├── README.md                   # this file
├── aws-sigv4/
│   └── get-vanilla/
│       ├── get-vanilla.req     # original HTTP request
│       ├── get-vanilla.creq    # canonical request (text)
│       ├── get-vanilla.sts     # string-to-sign (text)
│       └── provenance.txt      # source URL + license + cross-checks
├── rfc4506/
│   └── §4.10/
│       ├── three-bytes-with-pad.bin     # 8-byte verbatim example
│       └── provenance.txt
├── rfc5531/
│   └── §9-call-frame/
│       ├── nfsv4-null-call.bin          # 36-byte verbatim CALL message
│       └── provenance.txt
├── rfc1057/
│   └── §9.2-authsys/
│       ├── trivial-root-credential.bin  # 24-byte verbatim AUTH_SYS body
│       └── provenance.txt
```

## What goes in a fixture directory

- The **fixture file(s)** themselves — text or binary, named by their
  spec section (e.g. `get-vanilla.creq`, `three-bytes-with-pad.bin`).
- A **`provenance.txt`** sibling documenting:
  1. **Source** — URL or RFC section the bytes were copied from.
  2. **License** — under what license the upstream is distributed.
  3. **SHA-256** — of each fixture file, for the in-source sentinel
     test to compare against.
  4. **Cross-impl notes** — independent implementations that produce
     the same bytes (sanity check against transcription errors).

## What goes in the test source

For each fixture, the corresponding `crates/kiseki-gateway/tests/<rfc>.rs`
file should:
1. **Compare its own encoder output to the fixture bytes** — proving
   kiseki produces the same bytes the spec / external impl produces.
2. **Pin the fixture's SHA-256** — so a silent corruption / re-encoding
   of the fixture file surfaces as a test failure. Use `aws_lc_rs::digest`
   to compute on read; compare against the constant from `provenance.txt`.

## Why .gitattributes

Future binary wire captures (`.pcap` from a real `mount.nfs4` session)
should be Git LFS pointers, not committed bytes — they grow fast. Text
fixtures (`.creq`, `.sts`, `.req`, `.txt`) stay as plain files since
they diff cleanly.

## Adding a new fixture

1. Create `tests/wire-samples/<rfc>/<section>/`.
2. Vendor the fixture bytes from a verifiable source (RFC text or
   public test suite).
3. Write `provenance.txt` with source URL + license + SHA-256.
4. Add a regression test in `crates/kiseki-gateway/tests/<rfc>.rs`
   that asserts kiseki's encoder output equals the fixture bytes,
   AND a SHA-256-pinned read-back test.
