# S3 API

Kiseki exposes an S3-compatible HTTP gateway on port 9000 (configurable
via `KISEKI_S3_ADDR`). The gateway implements the subset of S3 API
operations needed by HPC/AI workloads (ADR-014). Unsupported operations
return `501 Not Implemented`.

## Endpoint

```
http://<node>:9000
```

In the Docker Compose development stack, the endpoint is
`http://localhost:9000`.

## Authentication

Kiseki supports **AWS Signature Version 4** authentication:

- **Authorization header** -- standard SigV4 signing for aws-cli, boto3,
  and other SDK clients.
- **Presigned URLs** -- planned for a future release (not yet
  implemented).

In development mode (Docker Compose), any access key and secret key
values are accepted.

## Supported Operations

### Bucket Operations

S3 buckets map to Kiseki namespaces. Creating a bucket creates a
tenant-scoped namespace; deleting a bucket deletes the namespace.

| Operation | S3 API | Notes |
|-----------|--------|-------|
| Create bucket | `PUT /{bucket}` | Maps to namespace creation |
| Delete bucket | `DELETE /{bucket}` | Maps to namespace deletion |
| Head bucket | `HEAD /{bucket}` | Existence check |
| List buckets | `GET /` | Per-tenant bucket listing |

### Object Operations

| Operation | S3 API | Notes |
|-----------|--------|-------|
| Put object | `PUT /{bucket}/{key}` | Single-part upload |
| Get object | `GET /{bucket}/{key}` | Including byte-range reads (`Range` header) |
| Head object | `HEAD /{bucket}/{key}` | Metadata retrieval |
| Delete object | `DELETE /{bucket}/{key}` | Tombstone or delete marker (versioning) |
| List objects | `GET /{bucket}?list-type=2` | ListObjectsV2 with prefix, delimiter, pagination |

### Multipart Upload

For objects larger than a single PUT (large datasets, model weights):

| Operation | S3 API | Notes |
|-----------|--------|-------|
| Create multipart upload | `POST /{bucket}/{key}?uploads` | Returns upload ID |
| Upload part | `PUT /{bucket}/{key}?partNumber={n}&uploadId={id}` | Upload one part |
| Complete multipart upload | `POST /{bucket}/{key}?uploadId={id}` | Assemble parts into final object |
| Abort multipart upload | `DELETE /{bucket}/{key}?uploadId={id}` | Clean up incomplete upload |
| List multipart uploads | `GET /{bucket}?uploads` | List in-progress uploads |
| List parts | `GET /{bucket}/{key}?uploadId={id}` | List parts of an in-progress upload |

### Versioning

| Operation | S3 API | Notes |
|-----------|--------|-------|
| Get object version | `GET /{bucket}/{key}?versionId={v}` | Specific version retrieval |
| List object versions | `GET /{bucket}?versions` | Version listing |
| Delete object version | `DELETE /{bucket}/{key}?versionId={v}` | Delete specific version |

### Conditional Operations

| Header | Direction | Notes |
|--------|-----------|-------|
| `If-None-Match` | Write | Conditional write (create-if-not-exists) |
| `If-Match` | Write | Conditional write (update-if-matches) |
| `If-Modified-Since` | Read | Conditional read |

## Examples

### aws-cli

```bash
# Set up environment
export AWS_ACCESS_KEY_ID=kiseki
export AWS_SECRET_ACCESS_KEY=kiseki
export AWS_DEFAULT_REGION=us-east-1
ENDPOINT="--endpoint-url http://localhost:9000"

# Bucket operations
aws $ENDPOINT s3 mb s3://datasets
aws $ENDPOINT s3 ls

# Upload a directory
aws $ENDPOINT s3 sync ./training-data/ s3://datasets/imagenet/

# Download a file
aws $ENDPOINT s3 cp s3://datasets/imagenet/train.tar /tmp/train.tar

# Multipart upload (automatic for large files)
aws $ENDPOINT s3 cp ./large-model.bin s3://datasets/models/gpt.bin

# List objects with prefix
aws $ENDPOINT s3 ls s3://datasets/imagenet/ --recursive

# Delete
aws $ENDPOINT s3 rm s3://datasets/imagenet/train.tar
```

### curl

```bash
# Create a bucket
curl -X PUT http://localhost:9000/my-bucket

# PUT an object
curl -X PUT http://localhost:9000/my-bucket/config.json \
     -H "Content-Type: application/json" \
     -d '{"epochs": 100, "batch_size": 32}'

# GET an object
curl -s http://localhost:9000/my-bucket/config.json

# HEAD an object (metadata only)
curl -I http://localhost:9000/my-bucket/config.json

# Byte-range read (first 1024 bytes)
curl -s http://localhost:9000/my-bucket/large-file.bin \
     -H "Range: bytes=0-1023"

# DELETE an object
curl -X DELETE http://localhost:9000/my-bucket/config.json

# List objects (ListObjectsV2)
curl -s "http://localhost:9000/my-bucket?list-type=2&prefix=models/"

# Delete a bucket
curl -X DELETE http://localhost:9000/my-bucket
```

### Python (boto3)

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9000",
    aws_access_key_id="kiseki",
    aws_secret_access_key="kiseki",
    region_name="us-east-1",
)

# Create bucket
s3.create_bucket(Bucket="training")

# Upload
s3.put_object(Bucket="training", Key="data.csv", Body=b"col1,col2\n1,2\n")

# Download
obj = s3.get_object(Bucket="training", Key="data.csv")
print(obj["Body"].read().decode())

# List
for item in s3.list_objects_v2(Bucket="training")["Contents"]:
    print(item["Key"], item["Size"])
```

## Bucket-to-Namespace Mapping

Every S3 bucket maps 1:1 to a Kiseki namespace within the authenticated
tenant's scope. Bucket names become namespace identifiers. Buckets from
different tenants are fully isolated -- two tenants can have buckets with
the same name without conflict.

Objects within a bucket map to Kiseki compositions. Each object version
corresponds to a sequence of deltas in the shard that owns the
namespace.

## Encryption Handling

Kiseki always encrypts all data (invariant I-K1). S3 server-side
encryption headers are handled as follows:

| Header | Behavior |
|--------|----------|
| SSE-S3 (`x-amz-server-side-encryption: AES256`) | Acknowledged, no-op. System encryption is always on. |
| SSE-KMS with matching ARN | Acknowledged if the ARN matches the tenant KMS config. |
| SSE-KMS with different ARN | Rejected. Tenants cannot specify arbitrary keys. |
| SSE-C (`x-amz-server-side-encryption-customer-*`) | Rejected. Kiseki manages encryption, not the client. |

## Limitations

The following S3 features are **not implemented**:

| Feature | Reason |
|---------|--------|
| Lifecycle policies | Kiseki has its own tiering and retention model |
| Event notifications (SNS/SQS) | Requires message bus integration |
| Presigned URLs | Planned for future release |
| Bucket policies / IAM | Kiseki uses its own IAM and policy model |
| CORS | Not relevant for HPC/AI workloads |
| Object Lock | Covered by Kiseki's retention hold mechanism |
| S3 Select | Out of scope |
| Replication configuration | Kiseki manages replication internally |
| Storage classes | Kiseki uses affinity pools, not S3 storage classes |
