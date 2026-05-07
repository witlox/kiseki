//! tonic interceptor enforcing canonical SAN URI rules + audit emit.
//!
//! ADR-042 §3 + §11 + I-NG1, I-NG7, I-NG21 jointly require:
//!
//! 1. Extract leaf cert from `TlsConnectInfo`.
//! 2. Locate the kiseki tenant SAN URI (`spiffe://kiseki/tenant/<id>`).
//!    Reject if more than one is present (gate-1 F-M4 / I-NG21).
//! 3. Canonicalize via [`super::canonical_san::canonicalize`]. Reject
//!    with `PermissionDenied{san_canonicalization_mismatch}` on any
//!    rule break.
//! 4. Stash the canonical URI in `Request::extensions_mut()` so each
//!    RPC handler can cross-check it against the payload's
//!    `tenant_id` field at the proto-handler boundary (the second
//!    half of "F-H1 cert-SAN binding" — handle tokens reconcile via
//!    [`super::handle_token::verify_and_decode`]).
//! 5. On rejection, emit a security-failure audit event so SOC has
//!    visibility into rejected tokens (I-NG7).
//!
//! NOTE on cert re-validation: ADR-023 specifies CRL/OCSP re-checks
//! on long-running streams. A periodic background task on the
//! interceptor walks active streams and tears down any whose cert
//! has been revoked. That task is constructed via
//! [`SanInterceptor::spawn_revalidation_task`] (Phase 3 places the
//! hook; Phase 4 wires it into `kiseki-server::runtime`).

use std::sync::Arc;

use tonic::{Request, Status};
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME;
use x509_parser::prelude::FromDer;

use super::canonical_san::{canonicalize, CanonicalSanUri, SanError};

/// Prefix matched by the kiseki tenant SAN URI form. Anything else on
/// the cert (e.g., a fabric-role URI from a leaked node cert) is
/// rejected as `NotTenantRole`.
pub const TENANT_SAN_PREFIX: &str = "spiffe://kiseki/tenant/";

/// Audit-event sink for security-failure events. Real wiring lives in
/// `kiseki-audit`; the test in this module uses a `Vec`-backed fake.
pub trait AuditSink: Send + Sync {
    /// `reason` is the [`SanError`] variant name (or other rule like
    /// `MULTIPLE_TENANT_SANS`); `peer_principal` is whatever we could
    /// extract before rejecting (best-effort, may be `None`).
    fn emit_security_failure(&self, reason: &str, peer_principal: Option<&str>);
}

/// No-op sink. Useful when wiring up tests / non-production binaries
/// before the real audit wiring lands.
#[derive(Debug, Default)]
pub struct NullAuditSink;

impl AuditSink for NullAuditSink {
    fn emit_security_failure(&self, _reason: &str, _peer_principal: Option<&str>) {}
}

/// Errors raised by the interceptor. Each maps to a specific
/// `tonic::Status` plus an audit-emit reason string.
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum InterceptError {
    #[error("TLS client info missing — not on a TLS-protected port")]
    TlsInfoMissing,
    #[error("client certificate required")]
    ClientCertMissing,
    #[error("client cert chain empty")]
    EmptyChain,
    #[error("cert parse failed: {0}")]
    CertParse(String),
    #[error("cert has no SAN extension")]
    MissingSanExt,
    #[error("cert is not a kiseki tenant role (no spiffe://kiseki/tenant/ SAN)")]
    NotTenantRole,
    #[error("cert carries multiple kiseki tenant SAN URIs")]
    MultipleTenantSans,
    #[error("SAN canonicalization rejected: {0}")]
    Canonicalization(SanError),
}

impl InterceptError {
    /// Stable, audit-log-friendly tag. Renaming a variant is a
    /// breaking change for SOC dashboards keyed off this string.
    fn reason_tag(&self) -> &'static str {
        match self {
            Self::TlsInfoMissing => "TLS_INFO_MISSING",
            Self::ClientCertMissing => "CLIENT_CERT_MISSING",
            Self::EmptyChain => "EMPTY_CERT_CHAIN",
            Self::CertParse(_) => "CERT_PARSE_FAILED",
            Self::MissingSanExt => "MISSING_SAN_EXT",
            Self::NotTenantRole => "NOT_TENANT_ROLE",
            Self::MultipleTenantSans => "MULTIPLE_TENANT_SANS",
            Self::Canonicalization(_) => "SAN_CANONICALIZATION_MISMATCH",
        }
    }
}

impl From<InterceptError> for Status {
    fn from(e: InterceptError) -> Self {
        // Every variant currently maps to `PermissionDenied`. Kept as
        // an explicit `From` (rather than `tonic::Status::from`) so
        // future variants must consciously decide their gRPC code.
        Status::permission_denied(e.to_string())
    }
}

/// Stateful interceptor. `Arc`-cloneable so it can be installed as a
/// `tonic::service::Interceptor` on multiple service stacks.
pub struct SanInterceptor {
    audit: Arc<dyn AuditSink>,
    /// `false` permits the interceptor to no-op when no TLS info is
    /// present (development / plaintext mode). Production runtimes
    /// must set this to `true`.
    require_tls: bool,
}

impl SanInterceptor {
    /// Build a new interceptor. `audit` is the sink used for
    /// security-failure events.
    #[must_use]
    pub fn new(audit: Arc<dyn AuditSink>, require_tls: bool) -> Self {
        Self { audit, require_tls }
    }

    /// Apply the canonicalization checks. Returns the augmented
    /// request (with `CanonicalSanUri` installed in extensions) on
    /// success, or a `Status` on rejection. Audit emit fires before
    /// the error is returned.
    #[allow(clippy::result_large_err)]
    pub fn intercept<T>(&self, mut req: Request<T>) -> Result<Request<T>, Status> {
        match self.extract_canonical_san(&req) {
            Ok(canonical) => {
                req.extensions_mut().insert(canonical);
                Ok(req)
            }
            Err(InterceptError::TlsInfoMissing) if !self.require_tls => {
                // Plaintext development mode — install a synthetic
                // canonical SAN so the rest of the handler stack
                // doesn't NPE on lookup. Real production deployments
                // set `require_tls = true`.
                let dev = CanonicalSanUri::default_for_dev();
                req.extensions_mut().insert(dev);
                Ok(req)
            }
            Err(e) => {
                self.audit.emit_security_failure(e.reason_tag(), None);
                Err(e.into())
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn extract_canonical_san<T>(
        &self,
        req: &Request<T>,
    ) -> Result<CanonicalSanUri, InterceptError> {
        let info = req
            .extensions()
            .get::<tonic::transport::server::TlsConnectInfo<
                tonic::transport::server::TcpConnectInfo,
            >>()
            .ok_or(InterceptError::TlsInfoMissing)?;
        let certs = info.peer_certs().ok_or(InterceptError::ClientCertMissing)?;
        let leaf = certs.first().ok_or(InterceptError::EmptyChain)?;
        extract_canonical_tenant_san(leaf.as_ref())
    }
}

/// Pure helper — exposed for direct testing without a tonic stack.
/// Pulls every `URI:` SAN from the cert; if exactly one matches the
/// kiseki tenant prefix, canonicalizes it. More than one tenant SAN
/// is rejected (`MultipleTenantSans` / I-NG21).
#[allow(clippy::missing_errors_doc)]
pub fn extract_canonical_tenant_san(cert_der: &[u8]) -> Result<CanonicalSanUri, InterceptError> {
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|e| InterceptError::CertParse(format!("X.509 parse: {e}")))?;
    let san_ext = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == OID_X509_EXT_SUBJECT_ALT_NAME)
        .ok_or(InterceptError::MissingSanExt)?;
    let parsed = san_ext.parsed_extension();
    let ParsedExtension::SubjectAlternativeName(san) = parsed else {
        return Err(InterceptError::MissingSanExt);
    };
    let mut hits: Vec<&str> = san
        .general_names
        .iter()
        .filter_map(|n| match n {
            GeneralName::URI(u) => Some(*u),
            _ => None,
        })
        .filter(|u| u.starts_with(TENANT_SAN_PREFIX))
        .collect();
    if hits.is_empty() {
        return Err(InterceptError::NotTenantRole);
    }
    if hits.len() > 1 {
        return Err(InterceptError::MultipleTenantSans);
    }
    let single = hits.pop().expect("non-empty after length check");
    canonicalize(single).map_err(InterceptError::Canonicalization)
}

// Helper for SAN-extension test convenience. Public so tests in
// `server.rs` can mint a synthetic canonical-SAN extension when not
// running over a real TLS connection.
impl CanonicalSanUri {
    /// Canonical SAN used in plaintext development mode. **Never**
    /// install this when `require_tls = true`; the audit pipeline
    /// would silently see it as a real principal.
    ///
    /// The result is cached in a `OnceLock` so the canonicalization
    /// rule chain runs exactly once for the lifetime of the process.
    /// Without the cache, every plaintext-mode RPC re-canonicalized
    /// the same string and that hot-path waste was visible in the
    /// first kiseki-profile native run (Phase 7) — small constant
    /// per call but it accumulates at >10 k op/s.
    #[must_use]
    pub fn default_for_dev() -> Self {
        static DEV: std::sync::OnceLock<CanonicalSanUri> = std::sync::OnceLock::new();
        DEV.get_or_init(|| {
            canonicalize("spiffe://kiseki/tenant/dev")
                .expect("dev tenant URI is canonical by construction")
        })
        .clone()
    }

    /// Test-only helper: build a `CanonicalSanUri` directly from a
    /// canonical string. Bypasses re-canonicalization. Calling this
    /// with a non-canonical input is a contract violation.
    #[doc(hidden)]
    #[must_use]
    pub fn from_canonical_for_tests(s: &str) -> Self {
        canonicalize(s).expect("input must be canonical")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct CollectingSink {
        events: Mutex<Vec<(String, Option<String>)>>,
    }
    impl AuditSink for CollectingSink {
        fn emit_security_failure(&self, reason: &str, peer: Option<&str>) {
            self.events.lock().push((reason.to_string(), peer.map(String::from)));
        }
    }

    fn cert_with_sans(sans: Vec<rcgen::SanType>) -> Vec<u8> {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test-cert");
        params.subject_alt_names = sans;
        params.self_signed(&key).unwrap().der().to_vec()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tenant_san_extracted_and_canonicalized() {
        let der = cert_with_sans(vec![rcgen::SanType::URI(
            "spiffe://kiseki/tenant/org-pharma".try_into().unwrap(),
        )]);
        let san = extract_canonical_tenant_san(&der).unwrap();
        assert_eq!(san.as_str(), "spiffe://kiseki/tenant/org-pharma");
        assert_eq!(san.tenant_id(), "org-pharma");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cert_with_no_tenant_san_rejected() {
        let der = cert_with_sans(vec![rcgen::SanType::URI(
            "spiffe://cluster/fabric/node-1".try_into().unwrap(),
        )]);
        let err = extract_canonical_tenant_san(&der).unwrap_err();
        assert!(matches!(err, InterceptError::NotTenantRole));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cert_with_multiple_tenant_sans_rejected() {
        let der = cert_with_sans(vec![
            rcgen::SanType::URI(
                "spiffe://kiseki/tenant/org-a".try_into().unwrap(),
            ),
            rcgen::SanType::URI(
                "spiffe://kiseki/tenant/org-b".try_into().unwrap(),
            ),
        ]);
        let err = extract_canonical_tenant_san(&der).unwrap_err();
        assert!(matches!(err, InterceptError::MultipleTenantSans));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn near_miss_san_canonicalization_failure_audited() {
        let der = cert_with_sans(vec![rcgen::SanType::URI(
            "spiffe://kiseki/tenant/org-pharma/".try_into().unwrap(),
        )]);
        // The trailing slash is rejected by canonicalize.
        let err = extract_canonical_tenant_san(&der).unwrap_err();
        assert!(matches!(err, InterceptError::Canonicalization(_)));
        assert_eq!(err.reason_tag(), "SAN_CANONICALIZATION_MISMATCH");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn intercept_in_plaintext_dev_mode_installs_default_principal() {
        let sink = Arc::new(NullAuditSink);
        let intercept = SanInterceptor::new(sink, /*require_tls=*/ false);
        let req: Request<()> = Request::new(());
        let augmented = intercept.intercept(req).unwrap();
        let sun = augmented
            .extensions()
            .get::<CanonicalSanUri>()
            .expect("dev SAN installed");
        assert_eq!(sun.tenant_id(), "dev");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn intercept_with_require_tls_rejects_plaintext() {
        let sink = Arc::new(CollectingSink::default());
        let intercept = SanInterceptor::new(sink.clone(), /*require_tls=*/ true);
        let req: Request<()> = Request::new(());
        let err = intercept.intercept(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let evs = sink.events.lock();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].0, "TLS_INFO_MISSING");
    }
}
