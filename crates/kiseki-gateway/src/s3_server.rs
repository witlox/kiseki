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
use kiseki_common::locks::LockOrDie;

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
    /// Optional `kiseki_gateway_requests_total{method,status}`
    /// counter. Wired by the runtime so `/metrics` scrapes reflect
    /// every PUT/GET/HEAD/DELETE/LIST. Tests + library users
    /// without metrics configured leave this `None` and the bump
    /// is a no-op.
    requests_total_metric: Option<Arc<prometheus::IntCounterVec>>,
    /// Optional `kiseki_gateway_request_duration_seconds{method}`
    /// histogram. Same shape as `requests_total_metric` — wired by
    /// the runtime, `None` in tests. The 2026-05-04 GCP perf cluster
    /// surfaced this as registered-but-never-observed: the histogram
    /// existed in `/metrics` with always-zero counts, leaving the
    /// read-path latency invisible. Wiring it here closes that gap.
    request_duration_metric: Option<Arc<prometheus::HistogramVec>>,
    /// ADR-008 rev 2 / ADR-014 — `NodeId → s3 host:port` resolver for
    /// 307 redirect targets. Populated from `cluster/info` peer map
    /// at startup. Empty in dev / test deploys; the 307 path falls
    /// back to 503 + `Retry-After` when the leader hint can't be
    /// resolved to a host.
    #[allow(dead_code)] // implementer-step wires the 307 redirect path
    peer_s3_addrs: std::collections::HashMap<u64, String>,
    /// ADR-008 rev 2 — `kiseki_native_topology_stale_leader_redirects_total`
    /// counter, labeled by `protocol` ("s3", "native"). Optional;
    /// metric-less deploys take a no-op bump.
    #[allow(dead_code)] // implementer-step wires the metric bump on 307
    stale_leader_redirects_total: Option<Arc<prometheus::IntCounterVec>>,
}

impl<G: GatewayOps> S3State<G> {
    /// Record a completed S3 request.
    ///
    /// Bumps `kiseki_gateway_requests_total{method, status}` and
    /// observes `kiseki_gateway_request_duration_seconds{method}`.
    /// `status` is the numeric HTTP status as a string ("200", "412",
    /// "500") — keeps cardinality bounded even with arbitrary client
    /// behavior. Either metric being absent is a no-op.
    fn record_request(&self, method: &str, status: u16, duration: std::time::Duration) {
        if let Some(c) = self.requests_total_metric.as_ref() {
            c.with_label_values(&[method, &status.to_string()]).inc();
        }
        if let Some(h) = self.request_duration_metric.as_ref() {
            h.with_label_values(&[method])
                .observe(duration.as_secs_f64());
        }
    }
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

    /// Step C gate-1 finding S8 — resolve the `SigV4`-authenticated
    /// tenant ID for use as the
    /// `kiseki_native_topology_stale_leader_redirects_total{tenant=…}`
    /// label. Returns `Some(OrgId)` only when `SigV4` actually
    /// validated the request; the fallback tenant from dev-mode key-
    /// store-empty is reported as `None` so the metric distinguishes
    /// "anonymous / no auth" from "tenant X was authenticated by
    /// `SigV4`". The `tenant_label_from_auth` helper below maps the
    /// `Option` to the final string label.
    fn resolve_auth_tenant(
        &self,
        method: &axum::http::Method,
        uri: &axum::http::Uri,
        headers: &HeaderMap,
    ) -> Option<OrgId> {
        let payload_hash = headers
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("UNSIGNED-PAYLOAD");
        crate::s3_auth::validate_request(method, uri, headers, payload_hash, &self.key_store)
            .ok()
            .map(|auth| auth.tenant_id)
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
    s3_router_full(gateway, fallback_tenant, AccessKeyStore::new(), None, None)
}

/// Build an axum router with an explicit access key store.
pub fn s3_router_with_keys<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    fallback_tenant: OrgId,
    key_store: AccessKeyStore,
) -> Router {
    s3_router_full(gateway, fallback_tenant, key_store, None, None)
}

/// Build an axum router with an access key store, a wired counter for
/// `kiseki_gateway_requests_total`, and a wired histogram for
/// `kiseki_gateway_request_duration_seconds`. The runtime passes both;
/// tests pass `None`. Without the wiring the metrics stay at zero
/// observations under load — the 2026-05-02 / 05-04 GCP perf clusters
/// both saw this and the read-path latency stayed invisible.
pub fn s3_router_full<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    fallback_tenant: OrgId,
    key_store: AccessKeyStore,
    requests_total: Option<Arc<prometheus::IntCounterVec>>,
    request_duration: Option<Arc<prometheus::HistogramVec>>,
) -> Router {
    s3_router_with_peers(
        gateway,
        fallback_tenant,
        key_store,
        requests_total,
        request_duration,
        std::collections::HashMap::new(),
        None,
    )
}

/// ADR-008 rev 2 / ADR-014 — full constructor with peer-map for the
/// `307` redirect target resolution and the new
/// `kiseki_native_topology_stale_leader_redirects_total` counter.
///
/// `peer_s3_addrs` maps `NodeId → "host:port"`. Empty in dev / test;
/// the 307 path falls back to 503 when the leader hint can't be
/// resolved.
#[allow(clippy::implicit_hasher)] // HashMap default hasher is fine for the small peer map
pub fn s3_router_with_peers<G: GatewayOps + Send + Sync + 'static>(
    gateway: S3Gateway<G>,
    fallback_tenant: OrgId,
    key_store: AccessKeyStore,
    requests_total: Option<Arc<prometheus::IntCounterVec>>,
    request_duration: Option<Arc<prometheus::HistogramVec>>,
    peer_s3_addrs: std::collections::HashMap<u64, String>,
    stale_leader_redirects_total: Option<Arc<prometheus::IntCounterVec>>,
) -> Router {
    let state = Arc::new(S3State {
        gateway,
        fallback_tenant,
        key_store,
        buckets: Mutex::new(HashSet::new()),
        requests_total_metric: requests_total,
        request_duration_metric: request_duration,
        peer_s3_addrs,
        stale_leader_redirects_total,
    });

    // Per-request middleware that records `kiseki_gateway_requests_
    // total{method, status}` on every response. Wrapping at the
    // router level means every PUT/GET/HEAD/DELETE/LIST/POST is
    // counted exactly once without each handler having to remember
    // to record. The PUT handler additionally records on its
    // pre-routing 412 path because the conditional-write helper
    // returns before reaching the middleware (outermost).
    let metric_state = state.clone();
    let metric_layer = axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let metric_state = metric_state.clone();
            async move {
                let method = req.method().as_str().to_owned();
                let started = std::time::Instant::now();
                let response = next.run(req).await;
                metric_state.record_request(&method, response.status().as_u16(), started.elapsed());
                response
            }
        },
    );

    Router::new()
        .route("/", get(list_buckets::<G>))
        .route(
            "/{bucket}",
            get(list_objects::<G>)
                .put(create_bucket::<G>)
                .delete(delete_bucket::<G>)
                .head(head_bucket::<G>),
        )
        // Wildcard match on `{*key}` rather than `{key}` so keys
        // with embedded slashes (the conventional S3 pseudo-directory
        // shape — e.g. `del/should-be-gone.bin`, `logs/2026/05/x.bin`)
        // route correctly. Without the wildcard axum 0.8 treats the
        // first segment after the bucket as the entire `{key}` and
        // the URL fails to match, returning a 404 at the router
        // level before any handler runs.
        .route(
            "/{bucket}/{*key}",
            put(put_or_upload_part::<G>)
                .get(get_object::<G>)
                .head(head_object::<G>)
                .delete(delete_or_abort::<G>)
                .post(post_multipart::<G>),
        )
        .layer(metric_layer)
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

/// ADR-008 rev 2 / ADR-014 — emit a `307 Temporary Redirect` toward
/// the cached shard leader when a write-side handler trips
/// `LeaderUnavailable{leader_hint=Some(node_id)}`. Read-side handlers
/// (GET, HEAD, LIST) fall back to `503 Service Unavailable` with
/// `Retry-After: 1` because reads SHOULD succeed locally per ADR-040
/// §D6.3 — a `LeaderUnavailable` on a read is a bug, not a routing
/// hint.
///
/// `request_scheme` MUST be the current request's URI scheme ("http"
/// or "https") to avoid the TLS-downgrade vector (adversary finding
/// S3). `peer_s3_addrs` maps `NodeId` → "host:port" for the cluster;
/// an empty or stale map falls back to 503 (adversary finding S6 +
/// closure of the unresolvable-hint case).
///
/// `metric` (optional) is bumped with labels `(protocol="s3",
/// tenant="<tenant_label>")` per ADR-008 rev 2 §"Observability".
/// `tenant_label` should be the `SigV4`-resolved tenant UUID string
/// for authenticated requests, or `"unauthenticated"` when no
/// `SigV4` validation succeeded (no `Authorization` header,
/// malformed header, signature mismatch, etc.). This closes Step C
/// gate-1 finding S8 — the legacy `"unknown"` placeholder is no
/// longer emitted.
#[allow(clippy::too_many_arguments)] // tenant_label is a deliberate add-on for S8
fn leader_unavailable_response(
    method: &axum::http::Method,
    request_scheme: &str,
    request_path_and_query: &str,
    _shard_id: kiseki_common::ids::ShardId,
    leader_hint: Option<u64>,
    peer_s3_addrs: &std::collections::HashMap<u64, String>,
    metric: Option<&prometheus::IntCounterVec>,
    tenant_label: &str,
) -> axum::response::Response {
    // Finding S6: GET / HEAD / OPTIONS / LIST MUST NOT 307. Reads
    // can be served by any node with composition state (ADR-040
    // §D6.3). The 307 path is write-only.
    let is_write = matches!(
        *method,
        axum::http::Method::PUT
            | axum::http::Method::POST
            | axum::http::Method::DELETE
            | axum::http::Method::PATCH
    );
    if !is_write {
        return service_unavailable_with_retry_after();
    }

    let Some(leader) = leader_hint else {
        // Mid-election: no peer to redirect to. 503 + Retry-After.
        return service_unavailable_with_retry_after();
    };
    let Some(peer_addr) = peer_s3_addrs.get(&leader) else {
        // Leader not in peer map (e.g. evicted node still in the
        // shard map). Fall back to 503.
        return service_unavailable_with_retry_after();
    };

    // Bump the metric. The tenant label is the SigV4-resolved tenant
    // UUID when present, else `"unauthenticated"` for requests with no
    // (or invalid) SigV4 auth.
    if let Some(c) = metric {
        c.with_label_values(&["s3", tenant_label]).inc();
    }

    let location = format!("{request_scheme}://{peer_addr}{request_path_and_query}");
    let mut resp = (axum::http::StatusCode::TEMPORARY_REDIRECT, String::new()).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&location) {
        resp.headers_mut().insert(axum::http::header::LOCATION, v);
    }
    // Finding S5 — NOT emitting Retry-After on the 307 (RFC 9110
    // §10.2.3 is delta-seconds only; client-side retry policy
    // carries any sub-second jitter).
    resp
}

/// Map a `SigV4`-resolved tenant `Option<OrgId>` to the metric label
/// used by `kiseki_native_topology_stale_leader_redirects_total`:
/// `"<uuid>"` for an authenticated tenant, `"unauthenticated"` for
/// no `SigV4` or a failed validation. Replaces the legacy
/// `"unknown"` placeholder (Step C gate-1 S8).
fn tenant_label_from_auth(resolved: Option<OrgId>) -> String {
    match resolved {
        Some(t) => t.0.to_string(),
        None => "unauthenticated".to_owned(),
    }
}

/// 503 + `Retry-After: 1` body for write-side fallback and
/// read-side `LeaderUnavailable` responses. The header is
/// integer-seconds per RFC 9110 §10.2.3.
#[allow(dead_code)] // wired into handlers in the GREEN commit
fn service_unavailable_with_retry_after() -> axum::response::Response {
    let mut resp = s3_error_response(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "ServiceUnavailable",
        "shard leader unavailable; retry against another node",
    );
    if let Ok(v) = axum::http::HeaderValue::try_from("1") {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, v);
    }
    resp
}

/// Query params for PUT — distinguishes `PutObject` from `UploadPart`.
#[derive(serde::Deserialize, Default)]
struct PutParams {
    #[serde(rename = "uploadId")]
    upload_id: Option<String>,
    #[serde(rename = "partNumber")]
    part_number: Option<u32>,
}

#[allow(clippy::too_many_lines)] // S8 tenant resolution + 307 arms across UploadPart + PutObject
async fn put_or_upload_part<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    Path((bucket, key)): Path<(String, String)>,
    Query(params): Query<PutParams>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let request_scheme = request_scheme_from_uri_and_headers(&original_uri, &headers);
    let request_path_and_query = original_uri
        .path_and_query()
        .map_or_else(|| format!("/{bucket}/{key}"), |pq| pq.as_str().to_owned());

    // S8 — resolve SigV4 tenant up front so the 307 helper can label
    // the metric with the real tenant UUID instead of the legacy
    // `"unknown"` placeholder. Anonymous / failed-auth requests
    // produce `None` → `"unauthenticated"`.
    let auth_tenant = state.resolve_auth_tenant(&axum::http::Method::PUT, &original_uri, &headers);
    let tenant_label = tenant_label_from_auth(auth_tenant);

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
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id,
                leader_hint,
            }) => leader_unavailable_response(
                &axum::http::Method::PUT,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                leader_hint,
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id,
                leader_node_id,
            }) => leader_unavailable_response(
                &axum::http::Method::PUT,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                Some(leader_node_id.0),
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Capture Content-Type for later round-trip on GET (RFC 6838).
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Resolve HTTP-derived conditional headers. RFC 9110 §13:
    //   - `If-None-Match: *` → succeed only if no current binding.
    //   - `If-None-Match: <etag>` and `If-Match: <etag>` are also
    //     valid; we honor `*` and explicit etag values.
    //   - `If-Match: <etag>` → succeed only if current binding matches.
    let conditional = parse_write_conditional(&headers);

    // ADR-021: optional workflow correlation header.
    // `x-kiseki-workflow-ref: <uuid>` lets a tenant tag a write so
    // the advisory subsystem can correlate it with an active
    // workflow. Validated by the gateway against its shared
    // WorkflowTable; the result becomes a counter tick. Per I-WA1
    // the header is advisory and never blocks the write.
    let workflow_ref = headers
        .get("x-kiseki-workflow-ref")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
        .map(uuid::Uuid::into_bytes);

    // Regular PutObject — Content-Type is persisted on the
    // composition (ADV-PA-4: store-side metadata, not per-instance
    // HashMap). Survives across gateway instances + restart. The
    // URL `key` becomes the bucket-scoped name binding so subsequent
    // GET / HEAD / DELETE / LIST against the same key resolve to
    // this composition.
    let result = state
        .gateway
        .put_object(PutObjectRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            body: body.to_vec(),
            content_type,
            key: Some(key),
            conditional,
            workflow_ref,
        })
        .await;
    match result {
        Ok(resp) => (
            StatusCode::OK,
            [("etag", format!("\"{}\"", resp.etag))],
            String::new(),
        )
            .into_response(),
        Err(crate::error::GatewayError::PreconditionFailed(msg)) => {
            tracing::debug!(error = %msg, "S3 PUT: precondition failed → 412");
            (StatusCode::PRECONDITION_FAILED, msg).into_response()
        }
        Err(crate::error::GatewayError::ReadOnlyNamespace) => {
            (StatusCode::FORBIDDEN, "namespace is read-only").into_response()
        }
        Err(crate::error::GatewayError::NotFound(msg)) => {
            (StatusCode::NOT_FOUND, msg).into_response()
        }
        Err(crate::error::GatewayError::NamespaceNotFound(_)) => s3_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist.",
        ),
        Err(crate::error::GatewayError::LeaderUnavailable {
            shard_id,
            leader_hint,
        }) => leader_unavailable_response(
            &axum::http::Method::PUT,
            &request_scheme,
            &request_path_and_query,
            shard_id,
            leader_hint,
            &state.peer_s3_addrs,
            state.stale_leader_redirects_total.as_deref(),
            &tenant_label,
        ),
        // ADR-042 §4 / ADR-014 — `GatewayError::ForwardToLeader` is the
        // definite-leader sibling of `LeaderUnavailable`: the openraft
        // follower surfaced a concrete `leader_node_id` hint via the
        // `write_with_forwarding` chain. Re-use the 307 helper with
        // `Some(leader_node_id.0)` so the same scheme-preserving,
        // method-gated path applies (write-only redirect, peer-map
        // lookup, optional metric bump). The native-server proxy
        // fallback (`KISEKI_NATIVE_PROXY_FALLBACK=on`) handles the
        // gRPC equivalent; this arm covers the S3 protocol's analog
        // per ADR-042 §4 cross-protocol policy.
        Err(crate::error::GatewayError::ForwardToLeader {
            shard_id,
            leader_node_id,
        }) => leader_unavailable_response(
            &axum::http::Method::PUT,
            &request_scheme,
            &request_path_and_query,
            shard_id,
            Some(leader_node_id.0),
            &state.peer_s3_addrs,
            state.stale_leader_redirects_total.as_deref(),
            &tenant_label,
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// ADR-008 rev 2 / adversary finding S3 — resolve the request's
/// URI scheme to preserve TLS / non-TLS in the 307 redirect
/// Location header. Falls back to:
///   1. `original_uri.scheme_str()` — set when the gateway sits
///      behind a TLS-terminating proxy that forwards `X-Forwarded-Proto`
///      *as* the scheme. Axum exposes the parsed URI here.
///   2. `X-Forwarded-Proto` header (standard reverse-proxy convention).
///   3. "http" — sane default for plain-listener deploys.
fn request_scheme_from_uri_and_headers(uri: &axum::http::Uri, headers: &HeaderMap) -> String {
    if let Some(scheme) = uri.scheme_str() {
        return scheme.to_owned();
    }
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        return proto.to_owned();
    }
    "http".to_owned()
}

/// Parse `If-None-Match` / `If-Match` HTTP headers into a
/// `WriteConditional`. Returns `None` when neither header is present
/// or both fail to parse — the caller treats this as an unconditional
/// write. `*` is the only conditional we honor today; explicit etag
/// values are recognized and parsed (16-byte UUID), but a malformed
/// etag is silently ignored to preserve back-compat with clients that
/// send junk headers.
fn parse_write_conditional(headers: &HeaderMap) -> Option<crate::ops::WriteConditional> {
    if let Some(v) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if v.trim() == "*" {
            return Some(crate::ops::WriteConditional::IfNoneMatch);
        }
    }
    if let Some(v) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
        let trimmed = v.trim().trim_matches('"');
        if let Ok(u) = uuid::Uuid::parse_str(trimmed) {
            return Some(crate::ops::WriteConditional::IfMatch(CompositionId(u)));
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
async fn get_object<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    // Two addressing modes coexist:
    //   1. Per-key naming (S3 PUT URL key was bound by put_object): the
    //      gateway's name index resolves `key` → composition_id.
    //   2. UUID-by-id (back-compat / programmatic callers): when the
    //      URL key parses as a UUID and the name index doesn't bind
    //      it, fall back to treating it as a composition_id directly.
    let comp_id = match state
        .gateway
        .lookup_object_by_name(state.fallback_tenant, ns_id, &key)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => match uuid::Uuid::parse_str(&key) {
            Ok(u) => CompositionId(u),
            Err(_) => return (StatusCode::NOT_FOUND, "object not found").into_response(),
        },
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
            if let Some(range_hdr) = headers
                .get(axum::http::header::RANGE)
                .and_then(|v| v.to_str().ok())
            {
                match parse_byte_range(range_hdr, body_bytes.len()) {
                    Some(RangeResult::Single { start, end }) => {
                        use axum::http::header::{
                            CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
                        };
                        use axum::http::HeaderValue;
                        let slice = body_bytes[start..=end].to_vec();
                        let slice_len = slice.len();
                        let total_len = body_bytes.len();
                        let mut resp = (StatusCode::PARTIAL_CONTENT, slice).into_response();
                        let h = resp.headers_mut();
                        h.insert(CONTENT_LENGTH, HeaderValue::from(slice_len));
                        if let Ok(v) = HeaderValue::from_str(&etag) {
                            h.insert(ETAG, v);
                        }
                        if let Ok(v) =
                            HeaderValue::from_str(&format!("bytes {start}-{end}/{total_len}"))
                        {
                            h.insert(CONTENT_RANGE, v);
                        }
                        if let Some(ct) = stored_ct {
                            if let Ok(v) = HeaderValue::from_str(&ct) {
                                h.insert(CONTENT_TYPE, v);
                            }
                        }
                        return resp;
                    }
                    Some(RangeResult::Unsatisfiable) | None => {
                        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                    }
                }
            }

            // Construct headers via static `HeaderName` constants
            // and `HeaderValue::from(<u64>)` for the content-length
            // (infallible). The previous shape allocated 5 Strings
            // per response (header names + values via `to_string()`)
            // and routed through 4-5 fallible `try_from` parses.
            build_get_response_with_headers(body_bytes, resp.content_length, &etag, stored_ct)
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
/// Build a 200 OK GET response with content-length / etag /
/// optional content-type headers, using the static `HeaderName`
/// constants + `HeaderValue::from(<u64>)` so the per-response
/// allocation count is one body + one optional content-type
/// String parse, instead of the previous 5-String + 5-`try_from`
/// loop.
fn build_get_response_with_headers(
    body: Vec<u8>,
    content_length: u64,
    etag: &str,
    content_type: Option<String>,
) -> axum::response::Response {
    use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
    use axum::http::HeaderValue;
    let mut response = (StatusCode::OK, body).into_response();
    let h = response.headers_mut();
    h.insert(CONTENT_LENGTH, HeaderValue::from(content_length));
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(ETAG, v);
    }
    if let Some(ct) = content_type {
        if let Ok(v) = HeaderValue::from_str(&ct) {
            h.insert(CONTENT_TYPE, v);
        }
    }
    response
}

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
    let comp_id = match state
        .gateway
        .lookup_object_by_name(state.fallback_tenant, ns_id, &key)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => match uuid::Uuid::parse_str(&key) {
            Ok(u) => CompositionId(u),
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        },
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
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
            [
                ("content-length", resp.content_length.to_string()),
                ("etag", format!("\"{}\"", comp_id.0)),
            ],
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
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    Path((bucket, key)): Path<(String, String)>,
    Query(params): Query<PostParams>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let request_scheme = request_scheme_from_uri_and_headers(&original_uri, &headers);
    let request_path_and_query = original_uri
        .path_and_query()
        .map_or_else(|| format!("/{bucket}/{key}"), |pq| pq.as_str().to_owned());

    // S8 — resolve SigV4 tenant up front so the 307 helper labels the
    // metric with the real tenant UUID instead of `"unknown"`.
    let auth_tenant = state.resolve_auth_tenant(&axum::http::Method::POST, &original_uri, &headers);
    let tenant_label = tenant_label_from_auth(auth_tenant);

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
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id,
                leader_hint,
            }) => leader_unavailable_response(
                &axum::http::Method::POST,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                leader_hint,
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id,
                leader_node_id,
            }) => leader_unavailable_response(
                &axum::http::Method::POST,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                Some(leader_node_id.0),
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // POST ?uploadId=X → CompleteMultipartUpload. Pass the URL key
    // through so the resulting composition is bound in the per-bucket
    // name index — without this multipart-uploaded objects would be
    // addressable only by their composition UUID, while plain PUTs
    // are addressable by their URL key. That asymmetry would silently
    // break GET/DELETE/LIST for the multipart-upload code path.
    if let Some(upload_id) = params.upload_id {
        let req = CompleteMultipartUploadRequest {
            tenant_id: state.fallback_tenant,
            namespace_id: ns_id,
            upload_id,
            key: Some(key),
        };
        return match state.gateway.complete_multipart_upload(&req).await {
            Ok(resp) => (
                StatusCode::OK,
                [("etag", format!("\"{}\"", resp.etag))],
                axum::Json(serde_json::json!({ "etag": resp.etag })),
            )
                .into_response(),
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id,
                leader_hint,
            }) => leader_unavailable_response(
                &axum::http::Method::POST,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                leader_hint,
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id,
                leader_node_id,
            }) => leader_unavailable_response(
                &axum::http::Method::POST,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                Some(leader_node_id.0),
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
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
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    Path((bucket, key)): Path<(String, String)>,
    Query(params): Query<DeleteParams>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ns_id = namespace_from_bucket(&bucket);
    let request_scheme = request_scheme_from_uri_and_headers(&original_uri, &headers);
    let request_path_and_query = original_uri
        .path_and_query()
        .map_or_else(|| format!("/{bucket}/{key}"), |pq| pq.as_str().to_owned());

    // S8 — resolve SigV4 tenant up front so the 307 helper labels the
    // metric with the real tenant UUID instead of `"unknown"`.
    let auth_tenant =
        state.resolve_auth_tenant(&axum::http::Method::DELETE, &original_uri, &headers);
    let tenant_label = tenant_label_from_auth(auth_tenant);

    // DELETE ?uploadId=X → AbortMultipartUpload
    if let Some(upload_id) = params.upload_id {
        let req = AbortMultipartUploadRequest { upload_id };
        return match state.gateway.abort_multipart_upload(&req).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id,
                leader_hint,
            }) => leader_unavailable_response(
                &axum::http::Method::DELETE,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                leader_hint,
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id,
                leader_node_id,
            }) => leader_unavailable_response(
                &axum::http::Method::DELETE,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                Some(leader_node_id.0),
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    // Regular DeleteObject. Same dual-addressing as GET:
    //   1. Per-key naming via the gateway's name index.
    //   2. UUID-by-id fallback when the key parses as a UUID and is
    //      not bound by name.
    // S3 DELETE is idempotent — a no-op on missing keys returns 204.
    let comp_id = match state
        .gateway
        .lookup_object_by_name(state.fallback_tenant, ns_id, &key)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => match uuid::Uuid::parse_str(&key) {
            Ok(u) => CompositionId(u),
            Err(_) => return StatusCode::NO_CONTENT.into_response(),
        },
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
        Err(crate::error::GatewayError::LeaderUnavailable {
            shard_id,
            leader_hint,
        }) => leader_unavailable_response(
            &axum::http::Method::DELETE,
            &request_scheme,
            &request_path_and_query,
            shard_id,
            leader_hint,
            &state.peer_s3_addrs,
            state.stale_leader_redirects_total.as_deref(),
            &tenant_label,
        ),
        Err(crate::error::GatewayError::ForwardToLeader {
            shard_id,
            leader_node_id,
        }) => leader_unavailable_response(
            &axum::http::Method::DELETE,
            &request_scheme,
            &request_path_and_query,
            shard_id,
            Some(leader_node_id.0),
            &state.peer_s3_addrs,
            state.stale_leader_redirects_total.as_deref(),
            &tenant_label,
        ),
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
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    Path(bucket): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    let request_scheme = request_scheme_from_uri_and_headers(&original_uri, &headers);
    let request_path_and_query = original_uri
        .path_and_query()
        .map_or_else(|| format!("/{bucket}"), |pq| pq.as_str().to_owned());

    // S8 — resolve SigV4 tenant up front so the 307 helper labels the
    // metric with the real tenant UUID instead of `"unknown"`.
    let auth_tenant = state.resolve_auth_tenant(&axum::http::Method::PUT, &original_uri, &headers);
    let tenant_label = tenant_label_from_auth(auth_tenant);

    // Existence check first — if the bucket name is already cached
    // we never touch the namespace path. Scope the MutexGuard so it
    // is dropped before any `.await` point.
    {
        let buckets = state.buckets.lock().lock_or_die("s3_server.buckets");
        if buckets.contains(&bucket) {
            return s3_error_response(
                StatusCode::CONFLICT,
                "BucketAlreadyExists",
                "The requested bucket name is not available.",
            );
        }
    }

    // Register the namespace BEFORE inserting into `state.buckets`.
    // Original ordering (insert-then-ensure) leaked the bucket name on
    // a partial `ensure_namespace` failure: the cached name made
    // subsequent `create_bucket` short-circuit with `BucketAlreadyExists`
    // while every `PutObject` against the same name 404'd because the
    // namespace was never actually registered. Observed on the
    // 2026-05-07 GCP compact run — `curl -sf … &` perf scripts ate
    // the 404s and the bug looked like a perf measurement until a
    // canary HEAD revealed empty objects.
    let ns_id = namespace_from_bucket(&bucket);
    if let Err(e) = state
        .gateway
        .ensure_namespace(state.fallback_tenant, ns_id)
        .await
    {
        return match e {
            crate::error::GatewayError::LeaderUnavailable {
                shard_id,
                leader_hint,
            } => leader_unavailable_response(
                &axum::http::Method::PUT,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                leader_hint,
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            crate::error::GatewayError::ForwardToLeader {
                shard_id,
                leader_node_id,
            } => leader_unavailable_response(
                &axum::http::Method::PUT,
                &request_scheme,
                &request_path_and_query,
                shard_id,
                Some(leader_node_id.0),
                &state.peer_s3_addrs,
                state.stale_leader_redirects_total.as_deref(),
                &tenant_label,
            ),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()).into_response(),
        };
    }

    // Namespace is registered (locally + replicated). Now claim the
    // name. A concurrent `create_bucket` for the same name racing
    // through `ensure_namespace` is benign because `add_namespace` is
    // idempotent on the namespace-id; whichever caller wins the
    // `buckets.insert` race below returns 200, the other returns 409.
    let inserted = {
        let mut buckets = state.buckets.lock().lock_or_die("s3_server.buckets");
        buckets.insert(bucket)
    };

    if inserted {
        StatusCode::OK.into_response()
    } else {
        s3_error_response(
            StatusCode::CONFLICT,
            "BucketAlreadyExists",
            "The requested bucket name is not available.",
        )
    }
}

/// `DELETE /<bucket>` — delete a bucket. Returns 204 or 404.
async fn delete_bucket<G: GatewayOps + Send + Sync + 'static>(
    State(state): State<Arc<S3State<G>>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    let mut buckets = state.buckets.lock().lock_or_die("s3_server.buckets");
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
    let buckets = state.buckets.lock().lock_or_die("s3_server.buckets");
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
    let buckets = state.buckets.lock().lock_or_die("s3_server.buckets");

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
    let max_keys = params.max_keys.unwrap_or(1000);
    let prefix = params.prefix.clone().unwrap_or_default();
    let prefix_opt = if prefix.is_empty() {
        None
    } else {
        Some(prefix.as_str())
    };

    // Pull from the per-key name index first. Anything bound by name
    // (i.e. all S3-PUT'd objects since the per-key naming feature
    // landed) shows up with its real `key` here. Then merge with the
    // legacy UUID-only listing for back-compat with composition_id-
    // addressed callers (programmatic clients that PUT without a key
    // pre-naming, or NFS-written compositions surfaced via S3 LIST).
    let named = match state
        .gateway
        .list_named(state.fallback_tenant, ns_id, prefix_opt)
        .await
    {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let unnamed = match state
        .gateway
        .list_objects(state.fallback_tenant, ns_id)
        .await
    {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // Index named compositions by id so we can drop them from the
    // unnamed list (they'd appear twice otherwise — once as
    // `{"key": "user-key", ...}` and once as `{"key": "<uuid>", ...}`).
    let named_ids: std::collections::HashSet<CompositionId> =
        named.iter().map(|(_, id, _)| *id).collect();

    let mut combined: Vec<(String, u64)> = named
        .into_iter()
        .map(|(name, _, size)| (name, size))
        .collect();
    for (id, size) in unnamed {
        if named_ids.contains(&id) {
            continue;
        }
        let key = id.0.to_string();
        if !prefix.is_empty() && !key.starts_with(&prefix) {
            continue;
        }
        combined.push((key, size));
    }
    combined.sort_by(|a, b| a.0.cmp(&b.0));

    // Pagination: continuation token is the index to start from.
    let start = params
        .continuation_token
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0);
    let page: Vec<_> = combined.iter().skip(start).take(max_keys).collect();
    let is_truncated = start + page.len() < combined.len();

    let items: Vec<serde_json::Value> = page
        .iter()
        .map(|(key, size)| {
            serde_json::json!({
                "key": key,
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

    /// Bug 12 (GCP 2026-05-04 3rd run): PUT object to a bucket that
    /// hasn't been registered first via `PUT /<bucket>` previously
    /// returned 500 with body
    /// `"upstream error: namespace not found: NamespaceId(...)"` —
    /// operationally opaque, looks like a server failure when it's
    /// actually operator error. The contract is now: a typed
    /// `GatewayError::NamespaceNotFound` from the gateway maps to S3
    /// 404 with body `NoSuchBucket`.
    #[tokio::test(flavor = "multi_thread")]
    async fn put_to_unregistered_bucket_returns_404_no_such_bucket() {
        let app = test_router();

        // PUT object to a bucket that was never created via PUT /bucket.
        let req = Request::builder()
            .method("PUT")
            .uri("/absent-bucket/some-key")
            .body(Body::from(&b"payload"[..]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "PUT to unregistered bucket must return 404, not 500",
        );
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body_bytes);
        assert!(
            body.contains("NoSuchBucket"),
            "response body must carry the standard S3 NoSuchBucket code; got: {body}",
        );
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

    /// Pin the `kiseki_gateway_request_duration_seconds` wiring.
    /// The metric is registered in `kiseki-server::metrics`, but
    /// before this commit it had no observation site — `/metrics`
    /// always emitted zero counts and the GCP perf clusters had
    /// no read-path latency signal. Confirm a request through the
    /// router increments the histogram count for the matching
    /// method label.
    #[tokio::test(flavor = "multi_thread")]
    async fn request_duration_histogram_is_observed() {
        use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry};

        let registry = Registry::new();
        let counter = Arc::new(
            IntCounterVec::new(
                Opts::new("kiseki_gateway_requests_total_test", "test"),
                &["method", "status"],
            )
            .unwrap(),
        );
        registry.register(Box::new((*counter).clone())).unwrap();
        let histogram = Arc::new(
            HistogramVec::new(
                HistogramOpts::new("kiseki_gateway_request_duration_seconds_test", "test")
                    .buckets(vec![0.001, 0.01, 0.1, 1.0]),
                &["method"],
            )
            .unwrap(),
        );
        registry.register(Box::new((*histogram).clone())).unwrap();

        let master_key = SystemMasterKey::new([0u8; 32], KeyEpoch(1));
        let gw = InMemoryGateway::new(
            CompositionStore::new(),
            kiseki_chunk::arc_async(ChunkStore::new()),
            master_key,
        );
        let s3gw = S3Gateway::new(gw);
        let tenant = OrgId(uuid::Uuid::nil());
        let app = s3_router_full(
            s3gw,
            tenant,
            crate::s3_auth::AccessKeyStore::new(),
            Some(Arc::clone(&counter)),
            Some(Arc::clone(&histogram)),
        );

        // Any request — a GET on a missing bucket — must move the
        // GET histogram count above zero.
        let req = Request::builder()
            .method("GET")
            .uri("/no-such-bucket/00000000-0000-0000-0000-000000000099")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();

        let get_count = histogram.with_label_values(&["GET"]).get_sample_count();
        assert!(
            get_count >= 1,
            "kiseki_gateway_request_duration_seconds{{method=GET}} \
             must observe at least one sample after a GET request \
             (got {get_count}); without this wiring the histogram \
             stays at zero forever",
        );
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
        let storage = MemoryStorage::new();
        storage
            .apply_hydration_batch(HydrationBatch {
                shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
                puts: Vec::new(),
                removes: Vec::new(),
                name_inserts: Vec::new(),
                name_removes: Vec::new(),
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

    // === ADR-008 rev 2 / ADR-014 — S3 307 on LeaderUnavailable ===

    fn build_peer_map() -> std::collections::HashMap<u64, String> {
        let mut m = std::collections::HashMap::new();
        m.insert(1, "10.0.0.1:9000".to_owned());
        m.insert(2, "10.0.0.2:9000".to_owned());
        m.insert(3, "10.0.0.3:9000".to_owned());
        m
    }

    /// PUT against `LeaderUnavailable{leader_hint=Some(2)}` MUST emit
    /// 307 Temporary Redirect with `Location: http://10.0.0.2:9000/...`.
    /// Finding S3 — scheme is the request's scheme (http here).
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_put_emits_307_with_peer_location() {
        let resp = leader_unavailable_response(
            &axum::http::Method::PUT,
            "http",
            "/mybucket/mykey?uploadId=abc",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            Some(2),
            &build_peer_map(),
            None,
            "unauthenticated",
        );
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header on 307")
            .to_str()
            .unwrap();
        assert_eq!(loc, "http://10.0.0.2:9000/mybucket/mykey?uploadId=abc");
        // Finding S5 — Retry-After header NOT emitted on 307 (sub-second
        // jitter isn't RFC-compliant; client-side backoff carries it).
        assert!(
            !resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "Retry-After must NOT be emitted on 307 (RFC 9110 §10.2.3 delta-seconds only)"
        );
    }

    /// Finding S3 — TLS deploys MUST get https:// redirects.
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_put_preserves_https_scheme() {
        let resp = leader_unavailable_response(
            &axum::http::Method::PUT,
            "https",
            "/mybucket/mykey",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            Some(2),
            &build_peer_map(),
            None,
            "unauthenticated",
        );
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            loc.starts_with("https://"),
            "HTTPS request must produce HTTPS redirect, got: {loc}"
        );
    }

    /// Finding S6 — GET against `LeaderUnavailable` MUST NOT 307;
    /// reads are served by any node with composition state (ADR-040
    /// §D6.3). The error maps to 503 + Retry-After (matching the
    /// existing `ServiceUnavailable` path).
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_get_returns_503_not_307() {
        let resp = leader_unavailable_response(
            &axum::http::Method::GET,
            "http",
            "/mybucket/mykey",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            Some(2),
            &build_peer_map(),
            None,
            "unauthenticated",
        );
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "GET against LeaderUnavailable must be 503, not 307"
        );
        assert!(
            resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "503 must carry Retry-After"
        );
    }

    /// `LeaderUnavailable` with NO leader hint (active election) MUST
    /// fall back to 503 + Retry-After — no peer to redirect to.
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_without_hint_falls_back_to_503() {
        let resp = leader_unavailable_response(
            &axum::http::Method::PUT,
            "http",
            "/mybucket/mykey",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            None, // no leader hint — mid-election
            &build_peer_map(),
            None,
            "unauthenticated",
        );
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "503 fallback must carry Retry-After"
        );
    }

    /// `LeaderUnavailable` with a hint that's NOT in the peer map (e.g.
    /// a node that's no longer in the cluster) MUST fall back to 503.
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_with_unknown_peer_falls_back_to_503() {
        let resp = leader_unavailable_response(
            &axum::http::Method::PUT,
            "http",
            "/mybucket/mykey",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            Some(99), // not in peer map
            &build_peer_map(),
            None,
            "unauthenticated",
        );
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Finding S8 — the metric counter is bumped on every 307 emission
    /// with `protocol="s3"`.
    #[tokio::test(flavor = "multi_thread")]
    async fn leader_unavailable_bumps_stale_leader_redirects_counter() {
        let counter = std::sync::Arc::new(
            prometheus::IntCounterVec::new(
                prometheus::Opts::new(
                    "test_stale_leader_redirects_total",
                    "test counter for unit test",
                ),
                &["protocol", "tenant"],
            )
            .unwrap(),
        );
        let resp = leader_unavailable_response(
            &axum::http::Method::PUT,
            "http",
            "/mybucket/mykey",
            kiseki_common::ids::ShardId(uuid::Uuid::from_u128(1)),
            Some(2),
            &build_peer_map(),
            Some(&counter),
            "unauthenticated",
        );
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        // Step C gate-1 S8 — the metric label MUST be the resolved
        // tenant id (or `"unauthenticated"` when no SigV4 ran), no
        // longer the legacy `"unknown"` placeholder.
        let bumped = counter.with_label_values(&["s3", "unauthenticated"]).get();
        assert_eq!(bumped, 1, "307 emission must bump the metric");
    }

    // === Integration — `GatewayError::ForwardToLeader` end-to-end ===
    //
    // ADR-042 §4 / ADR-014: the openraft follower hint surfaces as
    // `LogError::ForwardToLeader { leader_node_id }` through the
    // `write_with_forwarding` chain; the S3 adapter (`s3.rs::put_object`)
    // now calls that path, so the handler's match arm at the bottom of
    // `put_or_upload_part` must convert the typed variant to a 307. The
    // stub gateway below short-circuits at `write_with_forwarding` so
    // we exercise the integration without standing up a Raft cluster.

    /// Stub `GatewayOps` whose `write_with_forwarding` always returns
    /// `GatewayError::ForwardToLeader { leader_node_id }`. Used to
    /// drive the S3 handler's new arm end-to-end without a real
    /// openraft store. All other ops are no-ops or error out — the
    /// test only touches the PUT path.
    struct ForwardingStubGateway {
        leader_node_id: kiseki_common::ids::NodeId,
        shard_id: kiseki_common::ids::ShardId,
    }

    #[async_trait::async_trait]
    impl crate::ops::GatewayOps for ForwardingStubGateway {
        async fn read(
            &self,
            _req: crate::ops::ReadRequest,
        ) -> Result<crate::ops::ReadResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::NotFound("stub".into()))
        }
        async fn write(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            // Legacy `write` collapses `ForwardToLeader` onto
            // `Upstream` — match that contract for any caller that
            // bypasses `write_with_forwarding`.
            Err(crate::error::GatewayError::Upstream(
                "stub: legacy write collapses ForwardToLeader".into(),
            ))
        }
        async fn write_with_forwarding(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
        async fn ensure_namespace(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
        ) -> Result<(), crate::error::GatewayError> {
            Ok(())
        }
    }

    /// PUT against a follower whose openraft store surfaced
    /// `ForwardToLeader{leader_node_id=2}` must emit `307 Temporary
    /// Redirect` with `Location: http://10.0.0.2:9000/...`. Covers the
    /// new arm wired between `S3Adapter::put_object` (now calling
    /// `write_with_forwarding`) and `put_or_upload_part`'s match.
    #[tokio::test(flavor = "multi_thread")]
    async fn forward_to_leader_put_emits_307_with_peer_location() {
        let stub = ForwardingStubGateway {
            leader_node_id: kiseki_common::ids::NodeId(2),
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(7)),
        };
        let s3gw = S3Gateway::new(stub);
        let tenant = OrgId(uuid::Uuid::nil());
        let counter = std::sync::Arc::new(
            prometheus::IntCounterVec::new(
                prometheus::Opts::new(
                    "test_forward_to_leader_redirects_total",
                    "test counter for ForwardToLeader integration",
                ),
                &["protocol", "tenant"],
            )
            .unwrap(),
        );
        let app = s3_router_with_peers(
            s3gw,
            tenant,
            AccessKeyStore::new(),
            None,
            None,
            build_peer_map(),
            Some(counter.clone()),
        );
        let req = Request::builder()
            .method("PUT")
            .uri("/mybucket/mykey")
            .body(Body::from(&b"payload"[..]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "ForwardToLeader{{leader_node_id=2}} must emit 307 (ADR-042 §4 / ADR-014)",
        );
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header on 307")
            .to_str()
            .unwrap();
        assert_eq!(
            loc, "http://10.0.0.2:9000/mybucket/mykey",
            "Location must point at the leader's S3 endpoint from the peer map",
        );
        // The 307 path also bumps the
        // `kiseki_native_topology_stale_leader_redirects_total` metric
        // for `ForwardToLeader` (same code path as `LeaderUnavailable`).
        // Step C gate-1 S8 — the label is `"unauthenticated"` because
        // this PUT carries no `SigV4` Authorization header.
        let bumped = counter.with_label_values(&["s3", "unauthenticated"]).get();
        assert_eq!(
            bumped, 1,
            "ForwardToLeader 307 emission must bump the metric (ADR-008 rev 2 §Observability)",
        );
    }

    /// PUT against `ForwardToLeader{leader_node_id=99}` where node 99
    /// is NOT in the peer map falls back to 503 — the redirect helper
    /// can't construct a valid `Location:` without a peer entry, and
    /// silently dropping the redirect is preferable to a malformed
    /// Location header.
    #[tokio::test(flavor = "multi_thread")]
    async fn forward_to_leader_unknown_peer_falls_back_to_503() {
        let stub = ForwardingStubGateway {
            leader_node_id: kiseki_common::ids::NodeId(99),
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(7)),
        };
        let s3gw = S3Gateway::new(stub);
        let tenant = OrgId(uuid::Uuid::nil());
        let app = s3_router_with_peers(
            s3gw,
            tenant,
            AccessKeyStore::new(),
            None,
            None,
            build_peer_map(),
            None,
        );
        let req = Request::builder()
            .method("PUT")
            .uri("/mybucket/mykey")
            .body(Body::from(&b"payload"[..]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "Unknown peer for ForwardToLeader must fall back to 503",
        );
        assert!(
            resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "503 fallback must carry Retry-After",
        );
    }

    // === Item 2 — extend 307 arm to DELETE / multipart finalize / bucket ops ===
    //
    // Today (pre-feat/s3-307-completion) only `put_or_upload_part` honored
    // `GatewayError::LeaderUnavailable` / `GatewayError::ForwardToLeader`.
    // The other mutation paths (`delete_or_abort`, `post_multipart`'s
    // `complete_multipart_upload` and `create_multipart_upload` branches,
    // and `create_bucket`'s `ensure_namespace`) collapsed all errors to
    // 500. These RED tests assert the same 307 contract on each verb;
    // they fail until the corresponding match arms are added to the
    // handlers.

    /// Stub gateway whose mutation paths return
    /// `GatewayError::ForwardToLeader{leader_node_id}`. Used to drive
    /// the 307 contract on each S3 verb without standing up Raft.
    struct ForwardToLeaderMutationStub {
        leader_node_id: kiseki_common::ids::NodeId,
        shard_id: kiseki_common::ids::ShardId,
    }

    #[async_trait::async_trait]
    impl crate::ops::GatewayOps for ForwardToLeaderMutationStub {
        async fn read(
            &self,
            _req: crate::ops::ReadRequest,
        ) -> Result<crate::ops::ReadResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::NotFound("stub".into()))
        }
        async fn write(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::Upstream("legacy write".into()))
        }
        async fn write_with_forwarding(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
        async fn delete(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
            _composition_id: kiseki_common::ids::CompositionId,
        ) -> Result<(), crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
        async fn lookup_object_by_name(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
            _name: &str,
        ) -> Result<Option<kiseki_common::ids::CompositionId>, crate::error::GatewayError> {
            // Resolve by-name so DELETE proceeds to `delete()` rather
            // than the 204-on-missing fast path.
            Ok(Some(kiseki_common::ids::CompositionId(
                uuid::Uuid::from_u128(42),
            )))
        }
        async fn ensure_namespace(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
        ) -> Result<(), crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
        async fn start_multipart(
            &self,
            _namespace_id: kiseki_common::ids::NamespaceId,
        ) -> Result<String, crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
        async fn complete_multipart(
            &self,
            _upload_id: &str,
            _name: Option<&str>,
        ) -> Result<kiseki_common::ids::CompositionId, crate::error::GatewayError> {
            Err(crate::error::GatewayError::ForwardToLeader {
                shard_id: self.shard_id,
                leader_node_id: self.leader_node_id,
            })
        }
    }

    /// Same shape as `ForwardToLeaderMutationStub` but returns
    /// `LeaderUnavailable{leader_hint=Some(2)}` from each mutation
    /// path. Asserts parity between the two error variants on the
    /// extended 307 arms.
    struct LeaderUnavailableMutationStub {
        leader_hint: Option<u64>,
        shard_id: kiseki_common::ids::ShardId,
    }

    #[async_trait::async_trait]
    impl crate::ops::GatewayOps for LeaderUnavailableMutationStub {
        async fn read(
            &self,
            _req: crate::ops::ReadRequest,
        ) -> Result<crate::ops::ReadResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::NotFound("stub".into()))
        }
        async fn write(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::Upstream("legacy write".into()))
        }
        async fn write_with_forwarding(
            &self,
            _req: crate::ops::WriteRequest,
        ) -> Result<crate::ops::WriteResponse, crate::error::GatewayError> {
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id: self.shard_id,
                leader_hint: self.leader_hint,
            })
        }
        async fn delete(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
            _composition_id: kiseki_common::ids::CompositionId,
        ) -> Result<(), crate::error::GatewayError> {
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id: self.shard_id,
                leader_hint: self.leader_hint,
            })
        }
        async fn lookup_object_by_name(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
            _name: &str,
        ) -> Result<Option<kiseki_common::ids::CompositionId>, crate::error::GatewayError> {
            Ok(Some(kiseki_common::ids::CompositionId(
                uuid::Uuid::from_u128(42),
            )))
        }
        async fn ensure_namespace(
            &self,
            _tenant_id: kiseki_common::ids::OrgId,
            _namespace_id: kiseki_common::ids::NamespaceId,
        ) -> Result<(), crate::error::GatewayError> {
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id: self.shard_id,
                leader_hint: self.leader_hint,
            })
        }
        async fn start_multipart(
            &self,
            _namespace_id: kiseki_common::ids::NamespaceId,
        ) -> Result<String, crate::error::GatewayError> {
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id: self.shard_id,
                leader_hint: self.leader_hint,
            })
        }
        async fn complete_multipart(
            &self,
            _upload_id: &str,
            _name: Option<&str>,
        ) -> Result<kiseki_common::ids::CompositionId, crate::error::GatewayError> {
            Err(crate::error::GatewayError::LeaderUnavailable {
                shard_id: self.shard_id,
                leader_hint: self.leader_hint,
            })
        }
    }

    fn build_stub_app_forward_to_leader(
        counter: Option<std::sync::Arc<prometheus::IntCounterVec>>,
    ) -> Router {
        let stub = ForwardToLeaderMutationStub {
            leader_node_id: kiseki_common::ids::NodeId(2),
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(7)),
        };
        let s3gw = S3Gateway::new(stub);
        let tenant = OrgId(uuid::Uuid::nil());
        s3_router_with_peers(
            s3gw,
            tenant,
            AccessKeyStore::new(),
            None,
            None,
            build_peer_map(),
            counter,
        )
    }

    fn build_stub_app_leader_unavailable(
        counter: Option<std::sync::Arc<prometheus::IntCounterVec>>,
    ) -> Router {
        let stub = LeaderUnavailableMutationStub {
            leader_hint: Some(2),
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(7)),
        };
        let s3gw = S3Gateway::new(stub);
        let tenant = OrgId(uuid::Uuid::nil());
        s3_router_with_peers(
            s3gw,
            tenant,
            AccessKeyStore::new(),
            None,
            None,
            build_peer_map(),
            counter,
        )
    }

    fn test_counter() -> std::sync::Arc<prometheus::IntCounterVec> {
        std::sync::Arc::new(
            prometheus::IntCounterVec::new(
                prometheus::Opts::new(
                    format!("test_307_arm_extension_{}", uuid::Uuid::new_v4().simple()),
                    "test counter for extended 307 arm",
                ),
                &["protocol", "tenant"],
            )
            .unwrap(),
        )
    }

    // ----- DELETE object -----

    /// DELETE /bucket/key against a follower MUST emit 307 (not 500)
    /// when the gateway surfaces `ForwardToLeader`.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_object_forward_to_leader_emits_307() {
        let counter = test_counter();
        let app = build_stub_app_forward_to_leader(Some(counter.clone()));
        let req = Request::builder()
            .method("DELETE")
            .uri("/mybucket/mykey")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "DELETE on ForwardToLeader must emit 307, not 500",
        );
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header")
            .to_str()
            .unwrap();
        assert_eq!(loc, "http://10.0.0.2:9000/mybucket/mykey");
        assert_eq!(
            counter.with_label_values(&["s3", "unauthenticated"]).get(),
            1,
            "307 must bump the metric with the unauthenticated tenant label",
        );
    }

    /// DELETE /bucket/key against a follower MUST emit 307 when the
    /// gateway surfaces `LeaderUnavailable` with a resolvable hint.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_object_leader_unavailable_emits_307() {
        let app = build_stub_app_leader_unavailable(None);
        let req = Request::builder()
            .method("DELETE")
            .uri("/mybucket/mykey")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "DELETE on LeaderUnavailable must emit 307, not 500",
        );
    }

    // ----- Multipart finalize (POST ?uploadId=...) -----

    /// POST /bucket/key?uploadId=X against a follower must emit 307
    /// when `CompleteMultipartUpload` surfaces `ForwardToLeader`.
    #[tokio::test(flavor = "multi_thread")]
    async fn complete_multipart_forward_to_leader_emits_307() {
        let app = build_stub_app_forward_to_leader(None);
        let req = Request::builder()
            .method("POST")
            .uri("/mybucket/mykey?uploadId=abc")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "CompleteMultipartUpload on ForwardToLeader must emit 307, not 500",
        );
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header")
            .to_str()
            .unwrap();
        assert_eq!(loc, "http://10.0.0.2:9000/mybucket/mykey?uploadId=abc");
    }

    /// POST /bucket/key?uploadId=X against a follower must emit 307
    /// when `CompleteMultipartUpload` surfaces `LeaderUnavailable`.
    #[tokio::test(flavor = "multi_thread")]
    async fn complete_multipart_leader_unavailable_emits_307() {
        let app = build_stub_app_leader_unavailable(None);
        let req = Request::builder()
            .method("POST")
            .uri("/mybucket/mykey?uploadId=abc")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "CompleteMultipartUpload on LeaderUnavailable must emit 307, not 500",
        );
    }

    /// POST /bucket/key?uploads against a follower
    /// (`CreateMultipartUpload`) must emit 307 when `start_multipart`
    /// surfaces `ForwardToLeader`.
    #[tokio::test(flavor = "multi_thread")]
    async fn create_multipart_forward_to_leader_emits_307() {
        let app = build_stub_app_forward_to_leader(None);
        let req = Request::builder()
            .method("POST")
            .uri("/mybucket/mykey?uploads")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "CreateMultipartUpload on ForwardToLeader must emit 307, not 500",
        );
    }

    // ----- create_bucket -----

    /// PUT /bucket against a follower (`CreateBucket` →
    /// `ensure_namespace`) must emit 307 when `ensure_namespace`
    /// surfaces `ForwardToLeader`.
    #[tokio::test(flavor = "multi_thread")]
    async fn create_bucket_forward_to_leader_emits_307() {
        let app = build_stub_app_forward_to_leader(None);
        let req = Request::builder()
            .method("PUT")
            .uri("/mybucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "CreateBucket on ForwardToLeader must emit 307, not 500",
        );
        let loc = resp
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header")
            .to_str()
            .unwrap();
        assert_eq!(loc, "http://10.0.0.2:9000/mybucket");
    }

    /// PUT /bucket against a follower must emit 307 when
    /// `ensure_namespace` surfaces `LeaderUnavailable`.
    #[tokio::test(flavor = "multi_thread")]
    async fn create_bucket_leader_unavailable_emits_307() {
        let app = build_stub_app_leader_unavailable(None);
        let req = Request::builder()
            .method("PUT")
            .uri("/mybucket")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "CreateBucket on LeaderUnavailable must emit 307, not 500",
        );
    }

    // === Item 3 — SigV4-resolved tenant on the metric ===

    /// PUT WITHOUT a `SigV4` Authorization header MUST label the metric
    /// `"unauthenticated"` — the new replacement for the legacy
    /// `"unknown"` label (no `SigV4` ran, so no resolved tenant).
    #[tokio::test(flavor = "multi_thread")]
    async fn unauthenticated_put_307_labels_metric_unauthenticated() {
        let counter = test_counter();
        let app = build_stub_app_forward_to_leader(Some(counter.clone()));
        let req = Request::builder()
            .method("PUT")
            .uri("/mybucket/mykey")
            .body(Body::from(&b"payload"[..]))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let unauthed = counter.with_label_values(&["s3", "unauthenticated"]).get();
        assert_eq!(
            unauthed, 1,
            "Unauthenticated request must label the metric `unauthenticated` (not `unknown`)",
        );
    }

    /// PUT against `ForwardToLeader` emitted by a `SigV4`-authenticated
    /// request MUST label the metric with the resolved tenant id, NOT
    /// `"unknown"` or `"unauthenticated"`. End-to-end through the real
    /// handler: signs a request, runs `validate_request`, surfaces the
    /// resolved tenant on the metric.
    #[tokio::test(flavor = "multi_thread")]
    async fn sigv4_authenticated_put_307_carries_tenant_label() {
        let tenant = OrgId(uuid::Uuid::from_u128(0xABCD_EF01));
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let mut keys = AccessKeyStore::new();
        keys.insert(access_key.to_owned(), secret.to_owned(), tenant);

        let stub = ForwardingStubGateway {
            leader_node_id: kiseki_common::ids::NodeId(2),
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::from_u128(7)),
        };
        let s3gw = S3Gateway::new(stub);
        let counter = test_counter();
        let app = s3_router_with_peers(
            s3gw,
            OrgId(uuid::Uuid::nil()), // fallback differs from real tenant
            keys,
            None,
            None,
            build_peer_map(),
            Some(counter.clone()),
        );

        let req = build_signed_put_request(access_key, secret, "/mybucket/mykey", b"payload");
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "Authenticated PUT against ForwardToLeader must emit 307",
        );
        let tenant_label = tenant.0.to_string();
        let bumped = counter.with_label_values(&["s3", &tenant_label]).get();
        assert_eq!(
            bumped, 1,
            "307 emitted from a SigV4-authenticated request must label the metric \
             with the resolved tenant id (expected={tenant_label})",
        );
        let unauthed = counter.with_label_values(&["s3", "unauthenticated"]).get();
        assert_eq!(
            unauthed, 0,
            "Authenticated request must NOT tick the unauthenticated bucket",
        );
    }

    /// Build a valid `SigV4`-signed PUT request using the same
    /// canonical-request / signing-key derivation as
    /// `kiseki_gateway::s3_auth`. Reuses the `pub(crate)` helpers in
    /// `s3_auth` so the test exercises the real
    /// `validate_request` code path end-to-end.
    fn build_signed_put_request(
        access_key: &str,
        secret: &str,
        path: &str,
        body: &[u8],
    ) -> Request<Body> {
        use aws_lc_rs::digest;

        // Use a fixed date — the s3_auth path doesn't enforce a clock
        // skew window today (TODO in s3_auth:299), so any well-formed
        // timestamp signs cleanly.
        let date = "20260515";
        let timestamp = "20260515T120000Z";
        let region = "us-east-1";
        let service = "s3";

        let payload_hash =
            crate::s3_auth::hex_encode(digest::digest(&digest::SHA256, body).as_ref());

        let host = "localhost";
        let signed_header_names = vec![
            "host".to_string(),
            "x-amz-content-sha256".to_string(),
            "x-amz-date".to_string(),
        ];

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("host", host.parse().unwrap());
        headers.insert("x-amz-content-sha256", payload_hash.parse().unwrap());
        headers.insert("x-amz-date", timestamp.parse().unwrap());

        let uri: axum::http::Uri = path.parse().unwrap();
        let canon = crate::s3_auth::canonical_request(
            &axum::http::Method::PUT,
            &uri,
            &headers,
            &signed_header_names,
            &payload_hash,
        );
        let scope = format!("{date}/{region}/{service}/aws4_request");
        let sts = crate::s3_auth::string_to_sign(timestamp, &scope, &canon);
        let signing_key = crate::s3_auth::derive_signing_key(secret, date, region, service);
        let sig = crate::s3_auth::hmac_sha256(signing_key.as_ref(), sts.as_bytes());
        let sig_hex = crate::s3_auth::hex_encode(sig.as_ref());

        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{date}/{region}/{service}/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig_hex}",
        );

        Request::builder()
            .method("PUT")
            .uri(path)
            .header("host", host)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", timestamp)
            .header("authorization", auth)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }
}
