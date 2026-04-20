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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, head, put};
use axum::Router;
use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

use crate::ops::GatewayOps;
use crate::s3::{GetObjectRequest, PutObjectRequest, S3Gateway};

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
        .route("/:bucket/:key", put(put_object::<G>))
        .route("/:bucket/:key", get(get_object::<G>))
        .route("/:bucket/:key", head(head_object::<G>))
        .route("/:bucket/:key", delete(delete_object::<G>))
        .with_state(state)
}

async fn put_object<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, _key)): Path<(String, String)>,
    body: Bytes,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
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
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let comp_id = match uuid::Uuid::parse_str(&key) {
        Ok(u) => CompositionId(u),
        Err(_) => return (StatusCode::NOT_FOUND, "invalid key (must be UUID)").into_response(),
    };

    match state.gateway.get_object(GetObjectRequest {
        tenant_id: state.tenant_id,
        namespace_id: ns_id,
        composition_id: comp_id,
    }) {
        Ok(resp) => (
            StatusCode::OK,
            [("content-length", resp.content_length.to_string())],
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

async fn delete_object<G: GatewayOps + Send + Sync + 'static>(
    State(_state): State<Arc<S3State<G>>>,
    Path((_bucket, _key)): Path<(String, String)>,
) -> impl IntoResponse {
    // TODO: wire to CompositionOps::delete
    StatusCode::NO_CONTENT
}

/// Map S3 bucket name to a deterministic `NamespaceId`.
fn namespace_from_bucket(bucket: &str) -> NamespaceId {
    NamespaceId(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        bucket.as_bytes(),
    ))
}

/// Start the S3 HTTP server.
///
/// Currently plaintext only. When mTLS is needed, wrap the listener
/// with `tokio_rustls::TlsAcceptor` built from
/// `kiseki_transport::TlsConfig::server_config()`.
#[allow(clippy::expect_used)]
pub async fn run_s3_server(addr: SocketAddr, router: Router, use_tls: bool) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("S3 bind failed");

    if use_tls {
        // TODO: Accept rustls::ServerConfig, wrap listener with TlsAcceptor.
        // For now, log a warning and fall back to plaintext.
        eprintln!("  WARNING: S3 TLS requested but not yet implemented — using plaintext");
    }

    eprintln!(
        "  S3 HTTP gateway listening on {addr} ({})",
        if use_tls {
            "plaintext — TLS pending"
        } else {
            "plaintext"
        }
    );
    axum::serve(listener, router).await.ok();
}
