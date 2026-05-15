//! HTTP authentication / RBAC gating for the admin web surface.
//!
//! Background — there is no production HTTP auth on the metrics /
//! dashboard port today. The S3 gateway uses `SigV4`
//! (`kiseki-gateway::s3_auth`), the gRPC services use mTLS (ADR-014),
//! but the axum router on the metrics port is open. ADR-008 rev 2
//! §"Authorization" documents the operational expectation that
//! `/cluster/info` is "read-only public per deployment network
//! policy", and follow-up D1 in
//! `specs/findings/2026-05-15-ui-cli-followups.md` calls out the
//! `/admin/*` routes as relying on network-policy gating.
//!
//! This module ships the **stopgap** posture until full SSO / mTLS
//! integration lands:
//!
//! - `/admin/*` and `/ui/*` require a Bearer token matching the
//!   `KISEKI_ADMIN_TOKEN` env var (admin tier). Default ON; an
//!   explicit `KISEKI_ADMIN_AUTH_DISABLED=true` opts out for local
//!   development.
//! - `/cluster/info` requires *any* Bearer token (admin OR the
//!   secondary `KISEKI_CLIENT_TOKEN` env var, intended for clients
//!   that need topology bootstrap per ADR-008 rev 2 but should NOT
//!   have admin powers). Default ON; `KISEKI_CLUSTER_INFO_PUBLIC=true`
//!   opts out for deployments that genuinely want public read-only
//!   access (LB health probes, simple compose setups).
//! - Other routes (`/health`, `/metrics`, `/ui/logo`) stay open —
//!   they are already part of the unauthenticated probe surface.
//!
//! Token comparison is constant-time via `subtle`-style manual XOR
//! (no new dependency); the secrets are short.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// HTTP auth configuration. Built once at server boot from env vars.
///
/// `Clone` is cheap — `Arc<String>` interior. Stored on `UiState`.
#[derive(Clone, Debug, Default)]
pub struct AuthConfig {
    /// Admin token. Required to access `/admin/*` and `/ui/*`. When
    /// `None`, admin auth falls back to the dev-disabled / required-
    /// without-token error path.
    pub admin_token: Option<Arc<String>>,
    /// Client token. Required to access `/cluster/info` (alongside
    /// the admin token, which is also accepted). When `None`, only
    /// the admin token grants `/cluster/info`.
    pub client_token: Option<Arc<String>>,
    /// `KISEKI_ADMIN_AUTH_DISABLED=true` — operator explicitly turns
    /// off admin/UI auth (local development / single-machine compose).
    /// Default `false`.
    pub admin_auth_disabled: bool,
    /// `KISEKI_CLUSTER_INFO_PUBLIC=true` — operator explicitly opts
    /// in to unauthenticated `/cluster/info` (LB probes, public
    /// topology). Default `false`.
    pub cluster_info_public: bool,
}

impl AuthConfig {
    /// Build the auth config from environment variables.
    ///
    /// - `KISEKI_ADMIN_TOKEN` — admin Bearer token. Empty / unset =
    ///   admin auth is unconfigured.
    /// - `KISEKI_CLIENT_TOKEN` — non-admin Bearer token accepted on
    ///   `/cluster/info`. Empty / unset = only the admin token works.
    /// - `KISEKI_ADMIN_AUTH_DISABLED` — `true`/`1`/`yes` disables
    ///   admin/UI auth entirely.
    /// - `KISEKI_CLUSTER_INFO_PUBLIC` — `true`/`1`/`yes` makes
    ///   `/cluster/info` public.
    pub fn from_env() -> Self {
        let admin_token = std::env::var("KISEKI_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Arc::new);
        let client_token = std::env::var("KISEKI_CLIENT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Arc::new);
        let admin_auth_disabled = env_bool("KISEKI_ADMIN_AUTH_DISABLED");
        let cluster_info_public = env_bool("KISEKI_CLUSTER_INFO_PUBLIC");
        Self {
            admin_token,
            client_token,
            admin_auth_disabled,
            cluster_info_public,
        }
    }

    /// `true` when the presented bearer matches the admin token.
    /// Constant-time over the admin-token length to avoid trivial
    /// timing leaks; the secret is short so the cost is negligible.
    #[must_use]
    pub fn is_admin_token(&self, presented: &str) -> bool {
        match self.admin_token.as_deref() {
            Some(expected) => constant_time_eq(expected.as_bytes(), presented.as_bytes()),
            None => false,
        }
    }

    /// `true` when the presented bearer matches the client token OR
    /// the admin token. (`/cluster/info` accepts either — clients
    /// bootstrapping topology should not need full admin rights, but
    /// admin tooling shouldn't need a second token to discover the
    /// cluster.)
    #[must_use]
    pub fn is_client_or_admin_token(&self, presented: &str) -> bool {
        if self.is_admin_token(presented) {
            return true;
        }
        match self.client_token.as_deref() {
            Some(expected) => constant_time_eq(expected.as_bytes(), presented.as_bytes()),
            None => false,
        }
    }
}

fn env_bool(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref().map(str::to_ascii_lowercase),
        Ok(s) if s == "true" || s == "1" || s == "yes" || s == "on"
    )
}

/// Extract the `Authorization: Bearer <token>` header value, if any.
fn extract_bearer<B>(req: &Request<B>) -> Option<&str> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[allow(dead_code)] // wired into middleware in the GREEN commit
fn unauthorized(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"kiseki-admin\"")],
        format!("{{\"error\":\"unauthorized: {reason}\"}}"),
    )
        .into_response()
}

#[allow(dead_code)] // wired into middleware in the GREEN commit
fn forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        format!("{{\"error\":\"forbidden: {reason}\"}}"),
    )
        .into_response()
}

#[allow(dead_code)] // wired into middleware in the GREEN commit
fn misconfigured(reason: &str) -> Response {
    // 503 — auth is required by policy but the server has no token
    // to compare against. Distinct from 401 (missing client cred) so
    // operators see the misconfig in logs / alerts.
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("{{\"error\":\"auth misconfigured: {reason}\"}}"),
    )
        .into_response()
}

/// Middleware: require an admin Bearer token (or dev override).
///
/// Applied to `/admin/*` and `/ui/*` route groups.
///
/// Behaviour:
/// - `KISEKI_ADMIN_AUTH_DISABLED=true` → pass through unconditionally
///   (the request is logged via the standard axum trace layer; this
///   middleware does not add its own log line to avoid noisy dev
///   output).
/// - Otherwise: `Authorization: Bearer <token>` must match
///   `KISEKI_ADMIN_TOKEN`. Missing header → 401. Wrong token → 403.
///   No `KISEKI_ADMIN_TOKEN` set on the server → 503 (misconfig).
pub async fn admin_required(
    State(_cfg): State<AuthConfig>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // RED: stub passes everything through. Real implementation lands
    // in the follow-up commit; this lets the test file land first.
    next.run(req).await
}

/// Middleware: require any-authenticated principal (or public-mode
/// opt-out).
///
/// Applied to `/cluster/info`. Clients use this endpoint for topology
/// bootstrap per ADR-008 rev 2; they should not need admin powers,
/// hence accepting either `KISEKI_CLIENT_TOKEN` or
/// `KISEKI_ADMIN_TOKEN`.
///
/// Behaviour:
/// - `KISEKI_CLUSTER_INFO_PUBLIC=true` → pass through unconditionally.
/// - Otherwise: `Authorization: Bearer <token>` must match one of
///   the configured tokens. Missing → 401. Wrong → 403. Neither
///   token configured on server → 503 (misconfig).
pub async fn cluster_info_required(
    State(_cfg): State<AuthConfig>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // RED: stub passes everything through. Real implementation lands
    // in the follow-up commit; this lets the test file land first.
    next.run(req).await
}

/// Constant-time byte comparison. Returns `false` on length mismatch
/// (the lengths themselves are not a secret here — a token of the
/// wrong length is always wrong). For equal-length inputs the loop
/// touches every byte regardless of where the mismatch is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    //! Unit tests for the auth config and middleware.
    //!
    //! The middleware tests build a tiny axum Router with a stub
    //! handler and drive it through `tower::ServiceExt::oneshot`.
    //! That sidesteps having to spin up a real TCP listener while
    //! still exercising the real `Next` machinery.

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn cfg(admin: Option<&str>, client: Option<&str>) -> AuthConfig {
        AuthConfig {
            admin_token: admin.map(|s| Arc::new(s.to_owned())),
            client_token: client.map(|s| Arc::new(s.to_owned())),
            admin_auth_disabled: false,
            cluster_info_public: false,
        }
    }

    async fn handler_ok() -> &'static str {
        "ok"
    }

    fn admin_router(c: AuthConfig) -> Router {
        Router::new()
            .route("/admin/probe", get(handler_ok))
            .layer(axum::middleware::from_fn_with_state(c, admin_required))
    }

    fn cluster_router(c: AuthConfig) -> Router {
        Router::new().route("/cluster/info", get(handler_ok)).layer(
            axum::middleware::from_fn_with_state(c, cluster_info_required),
        )
    }

    async fn send(router: Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn get_req(uri: &str, bearer: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(tok) = bearer {
            b = b.header("authorization", format!("Bearer {tok}"));
        }
        b.body(Body::empty()).unwrap()
    }

    // -------------------- /admin/* --------------------

    #[tokio::test]
    async fn admin_route_rejects_missing_token_with_401() {
        let router = admin_router(cfg(Some("s3cret"), None));
        let (status, body) = send(router, get_req("/admin/probe", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("missing Authorization"), "body was: {body}");
    }

    #[tokio::test]
    async fn admin_route_rejects_wrong_token_with_403() {
        let router = admin_router(cfg(Some("s3cret"), None));
        let (status, _) = send(router, get_req("/admin/probe", Some("nope"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_route_accepts_correct_token() {
        let router = admin_router(cfg(Some("s3cret"), None));
        let (status, body) = send(router, get_req("/admin/probe", Some("s3cret"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn admin_route_with_dev_override_lets_anything_through() {
        let mut c = cfg(Some("s3cret"), None);
        c.admin_auth_disabled = true;
        let router = admin_router(c);
        let (status, _) = send(router, get_req("/admin/probe", None)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_route_returns_503_when_no_token_configured() {
        // Default ON + no token = 503 misconfig (operator must
        // pick: set the token OR opt out via the disable flag).
        let router = admin_router(cfg(None, None));
        let (status, body) = send(router, get_req("/admin/probe", Some("anything"))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("KISEKI_ADMIN_TOKEN"), "body was: {body}");
    }

    #[tokio::test]
    async fn admin_route_rejects_client_token() {
        // /cluster/info accepts client tokens; /admin/* does NOT —
        // that's the whole point of the two-tier ACL.
        let router = admin_router(cfg(Some("admin-tok"), Some("client-tok")));
        let (status, _) = send(router, get_req("/admin/probe", Some("client-tok"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // -------------------- /cluster/info --------------------

    #[tokio::test]
    async fn cluster_info_rejects_missing_token_with_401() {
        let router = cluster_router(cfg(Some("admin"), Some("client")));
        let (status, _) = send(router, get_req("/cluster/info", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cluster_info_accepts_client_token() {
        let router = cluster_router(cfg(Some("admin"), Some("client")));
        let (status, body) = send(router, get_req("/cluster/info", Some("client"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn cluster_info_also_accepts_admin_token() {
        // Admin tooling shouldn't need a second token just to call
        // /cluster/info during bootstrap.
        let router = cluster_router(cfg(Some("admin"), Some("client")));
        let (status, _) = send(router, get_req("/cluster/info", Some("admin"))).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn cluster_info_rejects_wrong_token() {
        let router = cluster_router(cfg(Some("admin"), Some("client")));
        let (status, _) = send(router, get_req("/cluster/info", Some("nope"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cluster_info_public_mode_lets_anything_through() {
        let mut c = cfg(Some("admin"), Some("client"));
        c.cluster_info_public = true;
        let router = cluster_router(c);
        let (status, _) = send(router, get_req("/cluster/info", None)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn cluster_info_returns_503_when_no_token_configured() {
        let router = cluster_router(cfg(None, None));
        let (status, body) = send(router, get_req("/cluster/info", Some("x"))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("KISEKI_"), "body was: {body}");
    }

    // -------------------- constant_time_eq + extractor --------------------

    #[test]
    fn constant_time_eq_handles_length_mismatch() {
        assert!(!constant_time_eq(b"short", b"longer-token"));
        assert!(!constant_time_eq(b"longer-token", b"short"));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sane"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn extract_bearer_handles_common_shapes() {
        let req = Request::builder()
            .header("authorization", "Bearer abc")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), Some("abc"));

        let req = Request::builder()
            .header("authorization", "Bearer   abc  ")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), Some("abc"));

        let req = Request::builder()
            .header("authorization", "Basic dXNlcjpwYXNz")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), None);

        let req: Request<()> = Request::builder().body(()).unwrap();
        assert_eq!(extract_bearer(&req), None);

        let req = Request::builder()
            .header("authorization", "Bearer ")
            .body(())
            .unwrap();
        assert_eq!(extract_bearer(&req), None);
    }

    // -------------------- env parsing --------------------

    #[test]
    fn env_bool_accepts_common_truthy_forms() {
        std::env::set_var("KISEKI_TEST_BOOL_X1", "true");
        std::env::set_var("KISEKI_TEST_BOOL_X2", "1");
        std::env::set_var("KISEKI_TEST_BOOL_X3", "YES");
        std::env::set_var("KISEKI_TEST_BOOL_X4", "on");
        std::env::set_var("KISEKI_TEST_BOOL_X5", "false");
        std::env::set_var("KISEKI_TEST_BOOL_X6", "");

        assert!(env_bool("KISEKI_TEST_BOOL_X1"));
        assert!(env_bool("KISEKI_TEST_BOOL_X2"));
        assert!(env_bool("KISEKI_TEST_BOOL_X3"));
        assert!(env_bool("KISEKI_TEST_BOOL_X4"));
        assert!(!env_bool("KISEKI_TEST_BOOL_X5"));
        assert!(!env_bool("KISEKI_TEST_BOOL_X6"));
        assert!(!env_bool("KISEKI_TEST_BOOL_X_UNSET"));

        for k in [
            "KISEKI_TEST_BOOL_X1",
            "KISEKI_TEST_BOOL_X2",
            "KISEKI_TEST_BOOL_X3",
            "KISEKI_TEST_BOOL_X4",
            "KISEKI_TEST_BOOL_X5",
            "KISEKI_TEST_BOOL_X6",
        ] {
            std::env::remove_var(k);
        }
    }
}
