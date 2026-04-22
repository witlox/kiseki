//! S3-compatible HTTP server via axum.
//!
//! Maps S3 REST API to `GatewayOps`. Runs as a separate listener
//! alongside the gRPC data-path server (ADR-019).
//!
//! MVP: PUT/GET/HEAD/DELETE on `/:bucket/:key`. No `SigV4` auth.
//! Supports optional mTLS when TLS files are configured.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::Router;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::ops::GatewayOps;
use crate::s3::{
    AbortMultipartUploadRequest, CompleteMultipartUploadRequest, CreateMultipartUploadRequest,
    DeleteObjectRequest, GetObjectRequest, PutObjectRequest, UploadPartRequest, S3Gateway,
};

/// Shared state for S3 HTTP handlers.
struct S3State<G: GatewayOps> {
    gateway: S3Gateway<G>,
    /// Bootstrap tenant (dev mode — production uses mTLS cert).
    tenant_id: OrgId,
}

/// Build an axum router for the S3 API.
pub fn s3_router<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    tenant_id: OrgId,
) -> Router {
    let state = Arc::new(S3State { gateway, tenant_id });

    Router::new()
        .route("/:bucket", get(list_objects::<G>))
        .route(
            "/:bucket/:key",
            put(put_or_upload_part::<G>)
                .get(get_object::<G>)
                .head(head_object::<G>)
                .delete(delete_or_abort::<G>)
                .post(post_multipart::<G>),
        )
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
    body: Bytes,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);

    // If uploadId + partNumber present, this is UploadPart.
    if let (Some(upload_id), Some(part_number)) = (params.upload_id, params.part_number) {
        let req = UploadPartRequest {
            tenant_id: state.tenant_id,
            namespace_id: ns_id,
            upload_id,
            part_number,
            body: body.to_vec(),
        };
        return match state.gateway.upload_part(&req) {
            Ok(resp) => (
                StatusCode::OK,
                [("etag", format!("\"{}\"", resp.etag))],
                String::new(),
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Regular PutObject.
    match state.gateway.put_object(PutObjectRequest {
        tenant_id: state.tenant_id,
        namespace_id: ns_id,
        body: body.to_vec(),
    }) {
        Ok(resp) => (
            StatusCode::OK,
            [("etag", format!("\"{}\"", resp.etag))],
            String::new(),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

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

    match state.gateway.get_object(GetObjectRequest {
        tenant_id: state.tenant_id,
        namespace_id: ns_id,
        composition_id: comp_id,
    }) {
        Ok(resp) => (
            StatusCode::OK,
            [
                ("content-length", resp.content_length.to_string()),
                ("etag", etag),
            ],
            resp.body,
        )
            .into_response(),
        Err(e) => {
            let code = if e.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, e.to_string()).into_response()
        }
    }
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

    match state.gateway.get_object(GetObjectRequest {
        tenant_id: state.tenant_id,
        namespace_id: ns_id,
        composition_id: comp_id,
    }) {
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
            tenant_id: state.tenant_id,
            namespace_id: ns_id,
        };
        return match state.gateway.create_multipart_upload(&req) {
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
            tenant_id: state.tenant_id,
            namespace_id: ns_id,
            upload_id,
        };
        return match state.gateway.complete_multipart_upload(&req) {
            Ok(resp) => (
                StatusCode::OK,
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
        return match state.gateway.abort_multipart_upload(&req) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Regular DeleteObject.
    let comp_id = match uuid::Uuid::parse_str(&key) {
        Ok(u) => CompositionId(u),
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };

    match state.gateway.delete_object(DeleteObjectRequest {
        tenant_id: state.tenant_id,
        namespace_id: ns_id,
        composition_id: comp_id,
    }) {
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
    match state.gateway.list_objects(state.tenant_id, ns_id) {
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
#[allow(clippy::expect_used)]
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
        eprintln!("  S3 HTTP gateway listening on {addr} (mTLS)");

        loop {
            let (tcp_stream, _peer) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  S3 accept error: {e}");
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
                            eprintln!("  S3 connection error: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("  S3 TLS handshake failed: {e}");
                    }
                }
            });
        }
    } else {
        eprintln!("  WARNING: S3 HTTP gateway listening on {addr} (PLAINTEXT — development only)");
        axum::serve(listener, router).await.ok();
    }
}
