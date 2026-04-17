# Kiseki — Go Coding Standards

Extends `.claude/guidelines/go.md` with project-specific conventions.

## Module

- Module path: `github.com/witlox/kiseki/control`
- Go module is the control plane only (tenancy, IAM, policy, federation, audit export, CLI)
- Data-path code is Rust — Go never touches hot paths

## Package Structure

```
control/
├── cmd/kiseki-control/    # Control plane API server
├── cmd/kiseki-cli/        # Admin CLI
├── pkg/tenant/            # Tenancy: org, project, workload
├── pkg/iam/               # IAM: Cluster CA, mTLS cert management, access requests
├── pkg/policy/            # Placement, quotas, compliance tags, retention holds
├── pkg/flavor/            # Flavor management, best-fit matching
├── pkg/federation/        # Cross-site config sync, data replication orchestration
├── pkg/audit/             # Audit export: tenant-scoped filtering, SIEM integration
├── pkg/discovery/         # Fabric-level discovery service support
└── proto/                 # Generated protobuf/gRPC (Go side)
```

## gRPC Boundary

- `google.golang.org/grpc` for server/client
- Proto definitions shared with Rust: `specs/architecture/proto/kiseki/v1/`
- Go-generated code in `control/proto/`
- All handlers validate tenant_id from mTLS cert context
- Zero-trust: cluster admin endpoints on management network only

## Error Handling

- Custom error types in `pkg/` matching `specs/architecture/error-taxonomy.md`
- Wrap: `fmt.Errorf("create tenant %s: %w", orgId, err)`
- gRPC status codes mapped from error types:
  - Retriable → `codes.Unavailable`
  - Permanent → `codes.FailedPrecondition` or `codes.NotFound`
  - Security → `codes.PermissionDenied` or `codes.Unauthenticated`

## Testing

- `godog` for BDD (Gherkin scenarios from `specs/features/control-plane.feature`)
- Step definitions in `tests/acceptance/`
- `testcontainers` for integration tests requiring external services
- `testify` for assertions
- Race detection in CI: `go test -race ./...`

## Domain Language

- All type names match `specs/ubiquitous-language.md`
- Go types mirror protobuf messages where applicable
- No abbreviations in exported names

## Configuration

- Declarative YAML/TOML config for control plane settings
- Environment variable overrides for deployment-specific values
- No config in code; all tunables externalized
