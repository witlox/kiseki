# ADR-014: S3 API Compatibility Scope

**Status**: Accepted
**Date**: 2026-04-17
**Context**: A-ADV-5 (S3 API compatibility scope)

## Decision

Implement a subset of S3 API covering the operations needed by HPC/AI
workloads. Not a complete S3 implementation.

### Supported (full)

| API | Notes |
|---|---|
| PutObject | Single-part upload |
| GetObject | Including byte-range reads |
| HeadObject | Metadata retrieval |
| DeleteObject | Tombstone or delete marker (versioning) |
| ListObjectsV2 | Prefix, delimiter, pagination |
| CreateMultipartUpload | |
| UploadPart | |
| CompleteMultipartUpload | |
| AbortMultipartUpload | |
| ListMultipartUploads | |
| ListParts | |
| CreateBucket | Maps to namespace creation |
| DeleteBucket | Maps to namespace deletion |
| HeadBucket | Existence check |
| ListBuckets | Per-tenant bucket listing |

### Supported (versioning)

| API | Notes |
|---|---|
| GetObjectVersion | Specific version retrieval |
| ListObjectVersions | Version listing |
| DeleteObjectVersion | Delete specific version |

### Supported (conditional)

| API | Notes |
|---|---|
| If-None-Match, If-Match | Conditional writes |
| If-Modified-Since | Conditional reads |

### Not supported (initial build)

| API | Reason | Future? |
|---|---|---|
| Lifecycle policies | Complex; competes with Kiseki's own tiering | Maybe |
| Event notifications | Requires message bus integration | Maybe |
| SSE-S3, SSE-KMS, SSE-C | Kiseki's encryption is always-on; S3 SSE headers are acknowledged but don't change behavior | N/A |
| Presigned URLs | Useful; add after core is stable | Yes |
| Bucket policies | Kiseki uses its own IAM/policy model | No |
| CORS | Not relevant for HPC/AI workloads | No |
| Object Lock | Covered by Kiseki's retention holds | Mapping possible |
| Select (S3 Select) | Out of scope | No |

### SSE header handling

S3 clients may send SSE headers. Kiseki always encrypts (I-K1).
- SSE-S3 headers: acknowledged, no-op (system encryption is always on)
- SSE-KMS headers with key ARN: if ARN matches tenant KMS config, acknowledged.
  If different: error (tenant can't specify arbitrary keys)
- SSE-C headers: rejected (Kiseki manages encryption, not the client)

## Consequences

- S3-compatible tooling (aws cli, boto3, rclone) works for supported operations
- Unsupported operations return 501 Not Implemented
- SSE headers are handled gracefully without breaking encryption model
