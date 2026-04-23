# Authentication

Kiseki uses a layered authentication model. The primary mechanism is
mTLS with certificates signed by a Cluster CA. Optional second-stage
authentication via tenant identity providers adds workload-level
authorization.

---

## mTLS with Cluster CA (I-Auth1)

The Cluster CA is the trust root for all data-fabric authentication.
Every participant in the data fabric (storage nodes, gateways, clients,
stream processors) presents a certificate signed by the Cluster CA.

### Certificate hierarchy

```
Cluster CA (managed by cluster admin)
  |
  +-- Server certificates (per storage node)
  |     SAN: node hostname, IP address
  |     OU: kiseki-server
  |
  +-- Key manager certificates (per key server)
  |     SAN: keyserver hostname, IP address
  |     OU: kiseki-keyserver
  |
  +-- Admin certificates (cluster admin)
  |     OU: kiseki-admin
  |
  +-- Tenant certificates (per tenant)
        SAN: tenant identifier
        OU: tenant-{org_id}
```

### Properties

- **No real-time auth server on data path** (I-Auth1). Certificates are
  local credentials. Authentication is a TLS handshake, not an RPC to
  a central authority. This eliminates a latency-sensitive dependency
  on the data path.
- **Per-tenant certificates**: Each tenant's clients and gateways
  present certificates that identify the tenant. The storage layer
  validates the certificate chain and extracts the tenant identity.
- **Certificate revocation**: Supported via CRL (`KISEKI_CRL_PATH`).
  The CRL is reloaded periodically. Revoked certificates are rejected
  at the TLS handshake.

### Configuration

```bash
# On storage nodes
KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
KISEKI_CERT_PATH=/etc/kiseki/tls/server.crt
KISEKI_KEY_PATH=/etc/kiseki/tls/server.key
KISEKI_CRL_PATH=/etc/kiseki/tls/crl.pem  # optional

# On client nodes
KISEKI_CA_PATH=/etc/kiseki/tls/ca.crt
KISEKI_CERT_PATH=/etc/kiseki/tls/client.crt
KISEKI_KEY_PATH=/etc/kiseki/tls/client.key
```

### Certificate generation example

```bash
# Generate Cluster CA (do this once)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout ca.key -out ca.crt -days 3650 -nodes \
  -subj "/CN=Kiseki Cluster CA"

# Generate server certificate
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout server.key -out server.csr -nodes \
  -subj "/CN=node1.example.com/OU=kiseki-server"

openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out server.crt -days 365 \
  -extfile <(echo "subjectAltName=DNS:node1.example.com,IP:10.0.0.1")

# Generate tenant client certificate
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout tenant.key -out tenant.csr -nodes \
  -subj "/CN=workload-1/OU=tenant-acme-corp"

openssl x509 -req -in tenant.csr -CA ca.crt -CAkey ca.key \
  -CAcreateserial -out tenant.crt -days 365
```

---

## SPIFFE SVID (I-Auth3)

SPIFFE (Secure Production Identity Framework for Everyone) is available
as an alternative to raw mTLS certificate management.

### SPIFFE ID structure

```
spiffe://kiseki.example.com/tenant/{org_id}/workload/{workload_id}
spiffe://kiseki.example.com/tenant/{org_id}/project/{project_id}/workload/{workload_id}
```

The SPIFFE ID maps directly to the tenant hierarchy
(organization/project/workload).

### SPIRE integration

SPIRE (the SPIFFE Runtime Environment) handles certificate issuance and
rotation automatically:

1. SPIRE Server acts as the Cluster CA (or delegates to it).
2. SPIRE Agent runs on each node (storage and compute).
3. Workloads receive SVIDs via the Workload API.
4. Certificates rotate automatically (no manual renewal).

### Benefits over raw mTLS

- Automatic certificate rotation (no manual renewal ceremonies).
- Workload attestation (verify the workload binary, not just the
  certificate).
- Short-lived certificates reduce the window of compromise.

---

## S3 SigV4 authentication

The S3 gateway supports AWS Signature Version 4 authentication for S3
API clients.

### How it works

1. The S3 client signs each request with an access key and secret key.
2. The gateway validates the signature.
3. The access key is mapped to a tenant identity via the control plane.
4. Subsequent authorization is based on the tenant identity.

### Configuration

Access keys are provisioned via the control plane:

```bash
grpcurl -d '{"tenant_id": "acme-corp", "workload_id": "training-job-1"}' \
  control:9200 kiseki.v1.ControlService/CreateS3Credentials
```

### Compatibility

The SigV4 implementation supports standard S3 clients:

```bash
# AWS CLI
aws --endpoint-url http://node1:9000 s3 ls

# boto3
import boto3
s3 = boto3.client('s3', endpoint_url='http://node1:9000',
                  aws_access_key_id='...', aws_secret_access_key='...')
```

---

## NFS authentication

The NFS gateway supports two authentication mechanisms:

### Kerberos (recommended for production)

NFSv4.2 with Kerberos provides strong authentication:

- `krb5` — Authentication only.
- `krb5i` — Authentication + integrity.
- `krb5p` — Authentication + integrity + privacy (encrypted).

The Kerberos principal maps to a tenant identity.

### AUTH_SYS (development only)

AUTH_SYS (traditional UNIX UID/GID authentication) is supported for
development and testing. It provides no real security and should not
be used in production. When AUTH_SYS is used, the NFS gateway maps
the export path to a tenant identity.

---

## OIDC/JWT second-stage authentication (I-Auth2)

Optional second-stage authentication validates workload identity against
the tenant admin's authorization. This provides an additional layer
beyond the mTLS "belongs to this cluster" identity.

### Architecture

```
Workload
  |
  v
mTLS (Cluster CA)  -->  "This workload belongs to tenant X"
  |
  v
OIDC/JWT (Tenant IdP)  -->  "This workload is authorized by tenant X's admin"
```

### Integration

1. Tenant admin configures their identity provider (Keycloak, Okta,
   Azure AD, etc.) in the control plane.
2. Workloads obtain JWT tokens from the tenant IdP.
3. On connection, the workload presents both:
   - mTLS certificate (Cluster CA trust chain)
   - JWT token (tenant IdP authorization)
4. The storage node validates both independently.

### Token validation

- JWT signature verification against the tenant IdP's JWKS endpoint.
- Token expiry and audience validation.
- Claims mapping to tenant hierarchy (org, project, workload).
- No real-time IdP dependency on the data path: JWKS keys are cached
  and refreshed periodically.

---

## gRPC role-based authorization

After authentication (mTLS + optional OIDC), gRPC services enforce
role-based authorization:

### Roles

| Role | Authentication | Access |
|------|---------------|--------|
| Cluster admin | Admin certificate (OU: kiseki-admin) | StorageAdminService, ControlService (full) |
| SRE (read-only) | SRE certificate | StorageAdminService (read-only: List*, Get*, Status) |
| Tenant admin | Tenant certificate + OIDC (optional) | ControlService (tenant-scoped), AuditExportService |
| Workload | Tenant certificate + OIDC (optional) | Data-path services, WorkflowAdvisoryService |

### Authorization enforcement

- **StorageAdminService**: Cluster admin only (mTLS cert with admin OU).
  SRE read-only role for monitoring.
- **ControlService**: Cluster admin for system operations, tenant admin
  for tenant-scoped operations.
- **Data-path services** (LogService, ChunkOps, CompositionOps,
  ViewOps): Any authenticated tenant workload, scoped to the tenant's
  own data.
- **WorkflowAdvisoryService**: Any authenticated tenant workload.
  Per-operation authorization (I-WA3): every request re-validates the
  caller's mTLS identity against the workflow's owning workload.

### Cluster admin isolation (I-T4)

The cluster admin certificate grants access to infrastructure
management but explicitly does NOT grant access to:

- Tenant configuration
- Tenant audit logs
- Tenant data (read or write)
- Tenant key material

Access to tenant resources requires an explicit access request approved
by the tenant admin.

---

## Client identity

### Client ID (native client)

Each native client process generates a stable identifier at startup:

- 128-bit CSPRNG value.
- Bound to the workload's mTLS certificate at first use.
- Scoped within (org, project, workload).
- Never reused across processes (I-WA4).

The client ID ties an operation stream to a single process instance.
It is not a user identity and not a session token.

### Workflow reference

For advisory-enabled workloads, a workflow reference is attached to
data-path RPCs as a gRPC binary metadata entry
(`x-kiseki-workflow-ref-bin`). This is a 16-byte opaque handle,
generated with 128+ bits of entropy, never reused, and verified
against the caller's mTLS identity on every request (I-WA3, I-WA10).
