//! S3-compatible HTTP server via axum.
//!
//! Maps S3 REST API to `GatewayOps`. Runs as a separate listener
//! alongside the gRPC data-path server (ADR-019).
//!
//! MVP: PUT/GET/HEAD/DELETE on `/:bucket/:key`. No `SigV4` auth.
//! Supports optional mTLS when TLS files are configured.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::Router;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::ops::GatewayOps;
use crate::s3::{
    AbortMultipartUploadRequest, CompleteMultipartUploadRequest, CreateMultipartUploadRequest,
    DeleteObjectRequest, GetObjectRequest, PutObjectRequest, S3Gateway, UploadPartRequest,
};
use crate::s3_auth::AccessKeyStore;

/// Shared state for S3 HTTP handlers.
struct S3State<G: GatewayOps> {
    gateway: S3Gateway<G>,
    /// Fallback tenant for unauthenticated requests (dev mode).
    fallback_tenant: OrgId,
    /// Access key store for `SigV4` authentication.
    #[allow(dead_code)] // wired when per-request auth middleware is added
    key_store: AccessKeyStore,
    /// In-memory bucket registry (namespace mapping).
    buckets: Mutex<HashSet<String>>,
}

impl<G: GatewayOps> S3State<G> {
    /// Resolve tenant from request headers (`SigV4`) or fall back to bootstrap.
    #[allow(dead_code)] // wired when per-request auth middleware is added
    fn resolve_tenant(
        &self,
        method: &axum::http::Method,
        uri: &axum::http::Uri,
        headers: &HeaderMap,
    ) -> OrgId {
        let payload_hash = headers
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("UNSIGNED-PAYLOAD");

        match crate::s3_auth::validate_request(method, uri, headers, payload_hash, &self.key_store)
        {
            Ok(auth) => {
                tracing::debug!(access_key = %auth.access_key, tenant_id = %auth.tenant_id.0, "S3 SigV4 authenticated");
                auth.tenant_id
            }
            Err(crate::s3_auth::AuthError::MissingAuth) if !self.key_store.is_empty() => {
                tracing::warn!("S3 request without Authorization header, using fallback tenant");
                self.fallback_tenant
            }
            Err(crate::s3_auth::AuthError::MissingAuth) => {
                // No key store configured — pure dev mode, use fallback.
                self.fallback_tenant
            }
            Err(e) => {
                tracing::warn!(error = %e, "S3 auth failed, using fallback tenant");
                self.fallback_tenant
            }
        }
    }
}

/// Build an axum router for the S3 API.
///
/// When `key_store` is non-empty, requests are authenticated via `SigV4`.
/// When empty (dev mode), all requests use `fallback_tenant`.
pub fn s3_router<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    fallback_tenant: OrgId,
) -> Router {
    s3_router_with_keys(gateway, fallback_tenant, AccessKeyStore::new())
}

/// Build an axum router with an explicit access key store.
pub fn s3_router_with_keys<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    fallback_tenant: OrgId,
    key_store: AccessKeyStore,
) -> Router {
    let state = Arc::new(S3State {
        gateway,
        fallback_tenant,
        key_store,
        buckets: Mutex::new(HashSet::new()),
    });

    Router::new()
        .route("/", get(list_buckets::<G>))
        .route(
            "/{bucket}",
            get(list_objects::<G>)
                .put(create_bucket::<G>)
                .delete(delete_bucket::<G>)
                .head(head_bucket::<G>),
        )
        .route(
            "/{bucket}/{key}",
            put(put_or_upload_part::<G>)
                .get(get_object::<G>)
                .head(head_object::<G>)
                .delete(delete_or_abort::<G>)
                .post(post_multipart::<G>),
        )
        // S3 single-PUT cap. AWS allows 5 GiB per PutObject; clients
        // chunk larger objects via multipart upload. axum's default
        // body limit (2 MiB) is far too small even for a small
        // training-dataset shard. Set to 5 GiB to match AWS while
        // letting the gateway's own multipart dispatch handle the
        // large-object path. Disabling the limit entirely is unsafe
        // (DoS via memory exhaustion).
        .layer(DefaultBodyLimit::max(5 * 1024 * 1024 * 1024))
        .with_state(state)
}

/// Query params for PUT — distinguishes `PutObject` from `UploadPart`.
#[derive(serde::Deserialize, Default)]
struct PutParams {
    #[serde(rename = "uploadId")]
    upload_id: Option<String>,
    #[serde(rename = "partNumber")]
    part_number: Option<u32>,
}

async fn put_or_upload_part<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, _key)): Path<(String, String)>,
    Query(params): Query<PutParams>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);

    // If uploadId + partNumber present, this is UploadPart.
    if let (Some(upload_id), Some(part_number)) = (params.upload_id, params.part_number) {
        let req = UploadPartRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            upload_id,
            part_number,
            body: body.to_vec(),
        };
        return match state.gateway.upload_part(&req).await {
            Ok(resp) => (
                StatusCode::OK,
                [("etag", format!("\"{}\"", resp.etag))],
                String::new(),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Capture Content-Type for later round-trip on GET (RFC 6838).
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Regular PutObject — Content-Type is persisted on the
    // composition (ADV-PA-4: store-side metadata, not per-instance
    // HashMap). Survives across gateway instances + restart.
    match state
        .gateway
        .put_object(PutObjectRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            body: body.to_vec(),
            content_type,
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            [("etag", format!("\"{}\"", resp.etag))],
            String::new(),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[allow(clippy::too_many_lines)]
async fn get_object<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let comp_id = match uuid::Uuid::parse_str(&key) {
        Ok(u) => CompositionId(u),
        Err(_) => return (StatusCode::NOT_FOUND, "invalid key (must be UUID)").into_response(),
    };

    let etag = format!("\"{}\"", comp_id.0);

    // Conditional: If-None-Match → 304 Not Modified.
    if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if inm == etag || inm == "*" {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    // Conditional: If-Match → 412 Precondition Failed.
    if let Some(im) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
        if im != etag && im != "*" {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
    }

    // RFC 9110 §13.1.3 — If-Modified-Since. kiseki does not yet
    // track per-object Last-Modified; treat "now" as the resource's
    // last-modified time. A header date in the future means "the
    // resource has not been modified since" — return 304.
    if let Some(ims) = headers
        .get("if-modified-since")
        .and_then(|v| v.to_str().ok())
    {
        if is_http_date_in_future(ims) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }
    // RFC 9110 §13.1.4 — If-Unmodified-Since. A header date in the
    // distant past means "the resource has been modified since" —
    // return 412.
    if let Some(ius) = headers
        .get("if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if is_http_date_in_past(ius) {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
    }

    match state
        .gateway
        .get_object(GetObjectRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            composition_id: comp_id,
        })
        .await
    {
        Ok(resp) => {
            // Content-Type is persisted on the composition (ADV-PA-4).
            // Both legs of the (bucket, key) URL are now backed by the
            // same `Composition.content_type` field, so multi-gateway
            // PUT→GET preserves the header.
            let stored_ct = resp.content_type.clone();
            let _ = (&bucket, &key); // path components retained for logs / future routing
            let body_bytes: Vec<u8> = resp.body;

            // RFC 9110 §14 — Range support.
            if let Some(range_hdr) = headers.get("range").and_then(|v| v.to_str().ok()) {
                match parse_byte_range(range_hdr, body_bytes.len()) {
                    Some(RangeResult::Single { start, end }) => {
                        let slice = body_bytes[start..=end].to_vec();
                        let mut hdrs = vec![
                            ("content-length".to_string(), slice.len().to_string()),
                            ("etag".to_string(), etag),
                            (
                                "content-range".to_string(),
                                format!("bytes {start}-{end}/{}", body_bytes.len()),
                            ),
                        ];
                        if let Some(ct) = stored_ct {
                            hdrs.push(("content-type".to_string(), ct));
                        }
                        let mut resp = (StatusCode::PARTIAL_CONTENT, slice).into_response();
                        let h = resp.headers_mut();
                        for (k, v) in hdrs {
                            if let (Ok(name), Ok(val)) = (
                                axum::http::HeaderName::try_from(k),
                                axum::http::HeaderValue::try_from(v),
                            ) {
                                h.insert(name, val);
                            }
                        }
                        return resp;
                    }
                    Some(RangeResult::Unsatisfiable) | None => {
                        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                    }
                }
            }

            let mut hdrs = vec![
                (
                    "content-length".to_string(),
                    resp.content_length.to_string(),
                ),
                ("etag".to_string(), etag),
            ];
            if let Some(ct) = stored_ct {
                hdrs.push(("content-type".to_string(), ct));
            }
            let mut response = (StatusCode::OK, body_bytes).into_response();
            let h = response.headers_mut();
            for (k, v) in hdrs {
                if let (Ok(name), Ok(val)) = (
                    axum::http::HeaderName::try_from(k),
                    axum::http::HeaderValue::try_from(v),
                ) {
                    h.insert(name, val);
                }
            }
            response
        }
        Err(crate::error::GatewayError::ServiceUnavailable(msg)) => {
            // ADR-040 §D6.3 + I-2: hydrator halt mode → 503 with
            // Retry-After. Load balancers route around this node;
            // the next request lands on a sibling whose hydrator is
            // still healthy.
            let mut resp =
                s3_error_response(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", &msg);
            if let Ok(v) = axum::http::HeaderValue::try_from("5") {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, v);
            }
            resp
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                s3_error_response(
                    StatusCode::NOT_FOUND,
                    "NoSuchKey",
                    "The specified key does not exist.",
                )
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        }
    }
}

/// RFC 9110 §14.1.2 — parsed Range request result.
enum RangeResult {
    Single { start: usize, end: usize },
    Unsatisfiable,
}

/// Parse an RFC 9110 §14.1.2 `Range:` header value. Supports
/// `bytes=N-M`, `bytes=-N` (suffix), and `bytes=N-`. Multi-range
/// (`bytes=A-B,C-D`) is rejected with `Unsatisfiable` — kiseki does
/// not implement `multipart/byteranges` responses.
fn parse_byte_range(value: &str, len: usize) -> Option<RangeResult> {
    let trimmed = value.trim();
    let spec = trimmed.strip_prefix("bytes=")?;
    if spec.contains(',') {
        // Multi-range — return Unsatisfiable so the caller emits 416.
        return Some(RangeResult::Unsatisfiable);
    }
    let (first, last) = spec.split_once('-')?;
    let (start, end) = if first.is_empty() {
        // Suffix range: bytes=-N → last N bytes.
        let n: usize = last.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(RangeResult::Unsatisfiable);
        }
        let start = len.saturating_sub(n);
        (start, len - 1)
    } else if last.is_empty() {
        let s: usize = first.parse().ok()?;
        if s >= len {
            return Some(RangeResult::Unsatisfiable);
        }
        (s, len - 1)
    } else {
        let s: usize = first.parse().ok()?;
        let e: usize = last.parse().ok()?;
        if s > e || s >= len {
            return Some(RangeResult::Unsatisfiable);
        }
        (s, e.min(len - 1))
    };
    Some(RangeResult::Single { start, end })
}

/// Test whether an RFC 9110 §5.6.7 HTTP-date string parses to a
/// time strictly after `now()`. Used by If-Modified-Since handling.
fn is_http_date_in_future(value: &str) -> bool {
    httpdate_to_epoch(value).is_some_and(|t| t > now_unix_secs())
}

/// Test whether an HTTP-date is strictly before `now()`. Used by
/// If-Unmodified-Since.
fn is_http_date_in_past(value: &str) -> bool {
    httpdate_to_epoch(value).is_some_and(|t| t < now_unix_secs())
}

/// Convert an HTTP-date (RFC 9110 §5.6.7 / RFC 7231) to a Unix
/// timestamp. Delegates to the `httpdate` crate which handles the
/// three legacy syntaxes (IMF-fixdate, RFC 850, asctime) the spec
/// allows. The previous in-house year-only parser failed for any
/// current-decade date (boto3, curl, etc.) — see ADV-PA-3.
fn httpdate_to_epoch(value: &str) -> Option<u64> {
    let parsed = httpdate::parse_http_date(value.trim()).ok()?;
    parsed
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// AWS S3-style XML error response.
///
/// Per the S3 REST API, error bodies are
/// `<?xml version="1.0" encoding="UTF-8"?><Error><Code>...</Code><Message>...</Message></Error>`.
/// Plain-text error bodies will confuse SDK clients that try to
/// parse the XML. Use this helper for any error path that AWS
/// documents an error code for.
fn s3_error_response(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <Error><Code>{code}</Code><Message>{message}</Message></Error>"
    );
    (status, [("content-type", "application/xml")], xml).into_response()
}

async fn head_object<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, key)): Path<(String, String)>,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let comp_id = match uuid::Uuid::parse_str(&key) {
        Ok(u) => CompositionId(u),
        Err(_) => return (StatusCode::NOT_FOUND).into_response(),
    };

    match state
        .gateway
        .get_object(GetObjectRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            composition_id: comp_id,
        })
        .await
    {
        Ok(resp) => (
            StatusCode::OK,
            [("content-length", resp.content_length.to_string())],
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Query params for POST — distinguishes `CreateMultipartUpload` from `CompleteMultipartUpload`.
#[derive(serde::Deserialize, Default)]
struct PostParams {
    uploads: Option<String>,
    #[serde(rename = "uploadId")]
    upload_id: Option<String>,
}

async fn post_multipart<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, _key)): Path<(String, String)>,
    Query(params): Query<PostParams>,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);

    // POST ?uploads → CreateMultipartUpload
    if params.uploads.is_some() {
        let req = CreateMultipartUploadRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
        };
        return match state.gateway.create_multipart_upload(&req).await {
            Ok(resp) => (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "uploadId": resp.upload_id })),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // POST ?uploadId=X → CompleteMultipartUpload
    if let Some(upload_id) = params.upload_id {
        let req = CompleteMultipartUploadRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            upload_id,
        };
        return match state.gateway.complete_multipart_upload(&req).await {
            Ok(resp) => (
                StatusCode::OK,
                [("etag", format!("\"{}\"", resp.etag))],
                axum::Json(serde_json::json!({ "etag": resp.etag })),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    (StatusCode::BAD_REQUEST, "missing ?uploads or ?uploadId").into_response()
}

/// Query params for DELETE — distinguishes `DeleteObject` from `AbortMultipartUpload`.
#[derive(serde::Deserialize, Default)]
struct DeleteParams {
    #[serde(rename = "uploadId")]
    upload_id: Option<String>,
}

async fn delete_or_abort<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, key)): Path<(String, String)>,
    Query(params): Query<DeleteParams>,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);

    // DELETE ?uploadId=X → AbortMultipartUpload
    if let Some(upload_id) = params.upload_id {
        let req = AbortMultipartUploadRequest { upload_id };
        return match state.gateway.abort_multipart_upload(&req).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Regular DeleteObject.
    let comp_id = match uuid::Uuid::parse_str(&key) {
        Ok(u) => CompositionId(u),
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };

    match state
        .gateway
        .delete_object(DeleteObjectRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            composition_id: comp_id,
        })
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            let code = if e.to_string().contains("not found") {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, e.to_string()).into_response()
        }
    }
}

// ── Bucket-level handlers (S3 5.2) ──────────────────────────────────

/// `PUT /<bucket>` — create a bucket. Returns 200 or 409.
async fn create_bucket<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path(bucket): Path<String>,
) -> axum::response::Response {
    // Check and insert bucket — scope the MutexGuard so it is dropped
    // before any `.await` point, keeping the future `Send`.
    let already_exists = {
        let mut buckets = state
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.contains(&bucket) {
            true
        } else {
            buckets.insert(bucket.clone());
            false
        }
    };

    if already_exists {
        return s3_error_response(
            StatusCode::CONFLICT,
            "BucketAlreadyExists",
            "The requested bucket name is not available.",
        );
    }

    // Register the namespace in the composition store so that subsequent
    // PUT object requests can find it (fixes "namespace not found" 500).
    let ns_id = namespace_from_bucket(&bucket);
    if let Err(e) = state
        .gateway
        .ensure_namespace(state.fallback_tenant, ns_id)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    StatusCode::OK.into_response()
}

/// `DELETE /<bucket>` — delete a bucket. Returns 204 or 404.
async fn delete_bucket<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    let mut buckets = state
        .buckets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if buckets.remove(&bucket) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `HEAD /<bucket>` — check bucket existence. Returns 200 or 404.
async fn head_bucket<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    let buckets = state
        .buckets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if buckets.contains(&bucket) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `GET /` — list all buckets. Returns XML `<ListAllMyBucketsResult>`.
async fn list_buckets<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
) -> impl IntoResponse {
    let buckets = state
        .buckets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListAllMyBucketsResult>\
         <Buckets>",
    );
    let mut sorted: Vec<&String> = buckets.iter().collect();
    sorted.sort();
    for name in sorted {
        xml.push_str("<Bucket><Name>");
        xml.push_str(name);
        xml.push_str("</Name></Bucket>");
    }
    xml.push_str("</Buckets></ListAllMyBucketsResult>");

    (StatusCode::OK, [("content-type", "application/xml")], xml)
}

/// Query parameters for `ListObjectsV2`.
#[derive(serde::Deserialize, Default)]
struct ListParams {
    prefix: Option<String>,
    #[serde(rename = "max-keys")]
    max_keys: Option<usize>,
    #[serde(rename = "continuation-token")]
    continuation_token: Option<String>,
}

async fn list_objects<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path(bucket): Path<String>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    match state
        .gateway
        .list_objects(state.fallback_tenant, ns_id)
        .await
    {
        Ok(objects) => {
            let max_keys = params.max_keys.unwrap_or(1000);
            let prefix = params.prefix.unwrap_or_default();

            // Filter by prefix.
            let filtered: Vec<_> = objects
                .into_iter()
                .filter(|(id, _)| id.0.to_string().starts_with(&prefix))
                .collect();

            // Pagination: continuation token is the index to start from.
            let start = params
                .continuation_token
                .and_then(|t| t.parse::<usize>().ok())
                .unwrap_or(0);
            let page: Vec<_> = filtered.iter().skip(start).take(max_keys).collect();
            let is_truncated = start + page.len() < filtered.len();

            let items: Vec<serde_json::Value> = page
                .iter()
                .map(|(id, size)| {
                    serde_json::json!({
                        "key": id.0.to_string(),
                        "size": size,
                    })
                })
                .collect();

            let mut body = serde_json::json!({
                "contents": items,
                "key_count": items.len(),
                "is_truncated": is_truncated,
            });

            if is_truncated {
                body["next_continuation_token"] = serde_json::json!((start + max_keys).to_string());
            }

            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Map S3 bucket name to a deterministic `NamespaceId`.
fn namespace_from_bucket(bucket: &str) -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        bucket.as_bytes(),
    ))
}

/// Start the S3 HTTP server with optional mTLS.
///
/// When `tls_config` is `Some`, requires mTLS client certs. When
/// `None`, serves plaintext (development only).
#[allow(clippy::expect_used, clippy::missing_panics_doc)]
pub async fn run_s3_server(
    addr: SocketAddr,
    router: Router,
    tls_config: Option<std::sync::Arc<rustls::ServerConfig>>,
) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("S3 bind failed");

    if let Some(tls) = tls_config {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        tracing::info!(addr = %addr, "S3 HTTP gateway listening (mTLS)");

        loop {
            let (tcp_stream, _peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "S3 accept error");
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let router = router.clone();
            tokio::spawn(async move {
                match acceptor.accept(tcp_stream).await {
                    Ok(tls_stream) => {
                        let io = hyper_util::rt::TokioIo::new(tls_stream);
                        let svc =
                            hyper_util::service::TowerToHyperService::new(router.into_service());
                        if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(io, svc)
                        .await
                        {
                            tracing::error!(error = %e, "S3 connection error");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "S3 TLS handshake failed");
                    }
                }
            });
        }
    } else {
        tracing::warn!(addr = %addr, "S3 HTTP gateway listening (PLAINTEXT — development only)");
        axum::serve(listener, router).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::mem_gateway::InMemoryGateway;
    use crate::s3::S3Gateway;
    use kiseki_chunk::store::ChunkStore;
    use kiseki_common::tenancy::KeyEpoch;
    use kiseki_composition::composition::CompositionStore;
    use kiseki_crypto::keys::SystemMasterKey;

    fn test_router() -> Router {
        let master_key = SystemMasterKey::new([0u8; 32], KeyEpoch(1));
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            master_key,
        );
        let s3gw = S3Gateway::new(gw);
        let tenant = OrgId(uuid::Uuid::nil());
        s3_router(s3gw, tenant)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_bucket_returns_200() {
        let app = test_router();
        let req = Request::builder()
            .method("PUT")
            .uri("/test-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_bucket_returns_409() {
        let app = test_router();

        // First create.
        let req = Request::builder()
            .method("PUT")
            .uri("/dup-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second create — should conflict.
        let req = Request::builder()
            .method("PUT")
            .uri("/dup-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn head_nonexistent_bucket_returns_404() {
        let app = test_router();
        let req = Request::builder()
            .method("HEAD")
            .uri("/no-such-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn head_existing_bucket_returns_200() {
        let app = test_router();

        // Create bucket first.
        let req = Request::builder()
            .method("PUT")
            .uri("/my-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // HEAD should find it.
        let req = Request::builder()
            .method("HEAD")
            .uri("/my-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_bucket_returns_204() {
        let app = test_router();

        // Create, then delete.
        let req = Request::builder()
            .method("PUT")
            .uri("/del-bucket")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method("DELETE")
            .uri("/del-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_nonexistent_bucket_returns_404() {
        let app = test_router();
        let req = Request::builder()
            .method("DELETE")
            .uri("/ghost-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_buckets_returns_xml() {
        let app = test_router();

        // Create two buckets.
        let req = Request::builder()
            .method("PUT")
            .uri("/alpha")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method("PUT")
            .uri("/beta")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // List.
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let xml = String::from_utf8(body.to_vec()).unwrap();
        assert!(xml.contains("<Name>alpha</Name>"), "xml: {xml}");
        assert!(xml.contains("<Name>beta</Name>"), "xml: {xml}");
        assert!(xml.contains("ListAllMyBucketsResult"), "xml: {xml}");
    }

    // ---------- S3 PutObject — empty body creates zero-byte object ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn put_empty_body_returns_200_with_etag() {
        let app = test_router();

        // Create bucket first.
        let req = Request::builder()
            .method("PUT")
            .uri("/empty-bucket")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // PUT with empty body.
        let req = Request::builder()
            .method("PUT")
            .uri("/empty-bucket/empty-key")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("etag").is_some(),
            "ETag should be returned for empty body PUT"
        );
    }

    // ---------- S3 GetObject — nonexistent returns 404 ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn get_nonexistent_object_returns_404() {
        let app = test_router();
        let req = Request::builder()
            .method("GET")
            .uri("/default/00000000-0000-0000-0000-000000000099")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---------- S3 GetObject — invalid UUID key returns 404 ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn get_invalid_uuid_returns_404() {
        let app = test_router();
        let req = Request::builder()
            .method("GET")
            .uri("/default/not-a-uuid")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---------- S3 HeadObject — metadata without body ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn head_object_returns_content_length_and_empty_body() {
        let app = test_router();

        // Create bucket.
        let req = Request::builder()
            .method("PUT")
            .uri("/head-bucket")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // PUT 100-byte object.
        let data = vec![0x42u8; 100];
        let req = Request::builder()
            .method("PUT")
            .uri("/head-bucket/some-key")
            .body(Body::from(data))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_owned();

        // HEAD by composition UUID.
        let req = Request::builder()
            .method("HEAD")
            .uri(format!("/head-bucket/{etag}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cl = resp
            .headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl, "100", "Content-Length should equal 100");

        // HEAD should have empty body.
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty(), "HEAD response body should be empty");
    }

    // ---------- S3 HeadObject — nonexistent returns 404 ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn head_nonexistent_object_returns_404() {
        let app = test_router();
        let req = Request::builder()
            .method("HEAD")
            .uri("/default/00000000-0000-0000-0000-000000000099")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---------- S3 DeleteObject — returns 204 ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_object_returns_204() {
        let app = test_router();
        let req = Request::builder()
            .method("DELETE")
            .uri("/default/anything")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // ---------- S3 ListObjectsV2 — prefix filtering ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn list_objects_prefix_filtering() {
        let app = test_router();

        // Create bucket + register namespace.
        let req = Request::builder()
            .method("PUT")
            .uri("/prefix-bucket")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // PUT several objects — keys are ignored, composition UUIDs are the real keys.
        // For prefix filtering to work, we need UUIDs that share a prefix.
        // Since UUIDs are random, we'll just verify the mechanism works
        // by creating objects and checking we can list them.
        let req = Request::builder()
            .method("PUT")
            .uri("/prefix-bucket/obj1")
            .body(Body::from("data1"))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        let req = Request::builder()
            .method("PUT")
            .uri("/prefix-bucket/obj2")
            .body(Body::from("data2"))
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // List all objects (no prefix filter).
        let req = Request::builder()
            .method("GET")
            .uri("/prefix-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let count = json["key_count"].as_u64().unwrap();
        assert_eq!(count, 2, "should have 2 objects");

        // List with a prefix that matches nothing.
        let req = Request::builder()
            .method("GET")
            .uri("/prefix-bucket?prefix=zzz")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let count = json["key_count"].as_u64().unwrap();
        assert_eq!(count, 0, "prefix=zzz should match nothing");
    }

    // ---------- S3 ListObjectsV2 — pagination with max-keys ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn list_objects_pagination() {
        let app = test_router();

        // Create bucket.
        let req = Request::builder()
            .method("PUT")
            .uri("/page-bucket")
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap();

        // PUT several objects.
        for i in 0..5 {
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/page-bucket/obj-{i}"))
                .body(Body::from(format!("data-{i}")))
                .unwrap();
            app.clone().oneshot(req).await.unwrap();
        }

        // List with max-keys=2.
        let req = Request::builder()
            .method("GET")
            .uri("/page-bucket?max-keys=2")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let count = json["key_count"].as_u64().unwrap();
        assert_eq!(count, 2, "should return max-keys=2 objects");
        assert!(
            json["is_truncated"].as_bool().unwrap(),
            "should be truncated"
        );
        assert!(
            json["next_continuation_token"].is_string(),
            "should provide NextContinuationToken"
        );
    }

    // ---------- Original roundtrip test ----------

    #[tokio::test(flavor = "multi_thread")]
    async fn put_get_object_roundtrip() {
        let app = test_router();

        // Create bucket first (registers namespace).
        let req = Request::builder()
            .method("PUT")
            .uri("/roundtrip-bucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // PUT object.
        let req = Request::builder()
            .method("PUT")
            .uri("/roundtrip-bucket/any-key")
            .body(Body::from("hello world"))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Extract etag (composition UUID) for GET.
        let etag = resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_owned();

        // GET object by composition UUID.
        let req = Request::builder()
            .method("GET")
            .uri(format!("/roundtrip-bucket/{etag}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"hello world");
    }

    /// Auditor finding A1 — verify the halt-mode path produces 503
    /// + Retry-After at the HTTP boundary (the gateway-side
    /// `ServiceUnavailable` mapping is tested in
    /// `mem_gateway::halt_mode_tests`).
    #[tokio::test(flavor = "multi_thread")]
    async fn get_object_returns_503_when_hydrator_halted() {
        use kiseki_composition::persistent::{CompositionStorage, HydrationBatch, MemoryStorage};

        // Build a CompositionStore whose backend reports halted=true.
        let mut storage = MemoryStorage::new();
        storage
            .apply_hydration_batch(HydrationBatch {
                puts: Vec::new(),
                removes: Vec::new(),
                new_last_applied_seq: kiseki_common::ids::SequenceNumber(0),
                stuck_state: Some(None),
                halted: Some(true),
            })
            .unwrap();
        let comp_store = CompositionStore::with_storage(Box::new(storage));

        let master_key = SystemMasterKey::new([0u8; 32], KeyEpoch(1));
        let gw = InMemoryGateway::new(
            comp_store,
            kiseki_chunk::arc_async(ChunkStore::new()),
            master_key,
        );
        let s3gw = S3Gateway::new(gw);
        let tenant = OrgId(uuid::Uuid::nil());
        let app = s3_router(s3gw, tenant);

        // Any GET — the comp_id doesn't have to exist; halt-mode
        // short-circuits before lookup-not-found.
        let etag = uuid::Uuid::new_v4();
        let req = Request::builder()
            .method("GET")
            .uri(format!("/halted-bucket/{etag}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // Retry-After header must be present so load balancers /
        // SDK clients treat this as "try elsewhere."
        assert!(
            resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "503 must include Retry-After header",
        );
    }
}
