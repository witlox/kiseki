//! OIDC/JWT validation for tenant identity (I-Auth2).
//!
//! Decodes and validates JWT tokens against per-tenant OIDC configuration.
//! Full JWKS signature verification is deferred (requires `jsonwebtoken`
//! crate, feature-gated). Currently validates structure, issuer, expiry,
//! and extracts claims via configurable mapping.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-tenant OIDC configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TenantIdpConfig {
    /// OIDC issuer URL (must match the `iss` claim).
    pub issuer_url: String,
    /// Expected audience (`aud` claim). If `None`, audience is not checked.
    pub audience: Option<String>,
    /// Mapping from JWT claims to Kiseki identity fields.
    pub claim_mapping: ClaimMapping,
    /// When `true`, accept tokens without cryptographic signature verification.
    ///
    /// # Warning
    ///
    /// JWKS signature verification is **not yet implemented** for RS256/ES256.
    /// Setting this to `false` (the default) causes `validate_jwt` to reject
    /// RS256/ES256 tokens until JWKS verification is available. HS256 tokens
    /// are verified via `shared_secret` regardless of this flag.
    #[serde(default)]
    pub unsafe_no_signature_verify: bool,
    /// Shared secret for HS256 signature verification.
    ///
    /// When set, tokens with `alg: HS256` are verified using HMAC-SHA256
    /// with this secret. Required for HS256 tokens.
    #[serde(default)]
    pub shared_secret: Option<String>,
}

/// Configurable mapping from JWT claim names to Kiseki identity fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaimMapping {
    /// JWT claim containing the organization ID.
    pub org_claim: String,
    /// JWT claim containing the project ID.
    pub project_claim: String,
    /// JWT claim containing the workload ID.
    pub workload_claim: String,
}

impl Default for ClaimMapping {
    fn default() -> Self {
        Self {
            org_claim: "org".into(),
            project_claim: "project".into(),
            workload_claim: "sub".into(),
        }
    }
}

/// Claims extracted from a validated JWT.
#[derive(Clone, Debug)]
pub struct ValidatedClaims {
    /// Organization ID extracted from the configured claim.
    pub org_id: String,
    /// Project ID extracted from the configured claim (optional).
    pub project_id: Option<String>,
    /// Workload ID extracted from the configured claim.
    pub workload_id: String,
    /// Token issuer (`iss` claim).
    pub issuer: String,
    /// Token expiry as Unix timestamp (`exp` claim).
    pub expires_at: u64,
}

/// JWT validation errors.
#[derive(Debug, thiserror::Error)]
pub enum IdpError {
    /// Token has expired.
    #[error("token expired")]
    TokenExpired,

    /// Issuer does not match the expected value.
    #[error("invalid issuer: expected {expected}, got {got}")]
    InvalidIssuer {
        /// Expected issuer from config.
        expected: String,
        /// Actual issuer from the token.
        got: String,
    },

    /// Audience does not match the expected value.
    #[error("invalid audience")]
    InvalidAudience,

    /// A required claim is missing from the token.
    #[error("missing claim: {0}")]
    MissingClaim(String),

    /// Token structure is invalid (malformed base64, JSON, etc.).
    #[error("invalid token: {0}")]
    InvalidToken(String),
}

/// Validate a JWT token against the given tenant IDP configuration.
///
/// This performs structural validation (base64 decode, JSON parse),
/// issuer verification, expiry check, and claim extraction.
///
/// # Warning — signature verification not implemented
///
/// JWKS signature verification is **not yet implemented**. Unless
/// `config.unsafe_no_signature_verify` is `true`, this function will
/// return an error. Callers must explicitly opt in to insecure mode.
#[must_use = "this result must be checked — signature verification is not implemented"]
pub fn validate_jwt(token: &str, config: &TenantIdpConfig) -> Result<ValidatedClaims, IdpError> {
    // JWT is header.payload.signature — we need the payload.
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(IdpError::InvalidToken(
            "expected 3 dot-separated parts".into(),
        ));
    }

    // Parse header to extract algorithm.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|e| IdpError::InvalidToken(format!("header base64 decode failed: {e}")))?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| IdpError::InvalidToken(format!("header JSON parse failed: {e}")))?;
    let alg = header
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none");

    // Reject alg=none — unsigned tokens are never acceptable.
    if alg.eq_ignore_ascii_case("none") {
        return Err(IdpError::InvalidToken("alg=none not allowed".into()));
    }

    // Only allow known algorithms.
    if !matches!(alg, "HS256" | "RS256" | "ES256") {
        return Err(IdpError::InvalidToken(format!(
            "unsupported algorithm: {alg}"
        )));
    }

    // Verify signature based on algorithm.
    match alg {
        "HS256" => {
            let secret = config.shared_secret.as_deref().ok_or_else(|| {
                IdpError::InvalidToken("HS256 token but no shared_secret configured".into())
            })?;
            verify_hs256(parts[0], parts[1], parts[2], secret)?;
        }
        "RS256" | "ES256" => {
            // JWKS verification not yet implemented for RS256/ES256.
            if !config.unsafe_no_signature_verify {
                return Err(IdpError::InvalidToken(
                    "signature verification required but JWKS not yet implemented".into(),
                ));
            }
            tracing::warn!(
                alg = alg,
                "JWT signature verification not implemented for {alg} \
                 — accepting token without cryptographic proof"
            );
        }
        _ => unreachable!(), // Covered by the allowlist check above.
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| IdpError::InvalidToken(format!("base64 decode failed: {e}")))?;

    let claims: serde_json::Value = serde_json::from_slice(&payload_bytes)
        .map_err(|e| IdpError::InvalidToken(format!("JSON parse failed: {e}")))?;

    // Validate issuer.
    let issuer = claims
        .get("iss")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| IdpError::MissingClaim("iss".into()))?;

    if issuer != config.issuer_url {
        return Err(IdpError::InvalidIssuer {
            expected: config.issuer_url.clone(),
            got: issuer.into(),
        });
    }

    // Validate audience (if configured).
    if let Some(ref expected_aud) = config.audience {
        let aud_valid = match claims.get("aud") {
            Some(serde_json::Value::String(aud)) => aud == expected_aud,
            Some(serde_json::Value::Array(auds)) => auds
                .iter()
                .any(|a| a.as_str().is_some_and(|s| s == expected_aud)),
            _ => false,
        };
        if !aud_valid {
            return Err(IdpError::InvalidAudience);
        }
    }

    // Validate expiry.
    let exp = claims
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| IdpError::MissingClaim("exp".into()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if exp <= now {
        return Err(IdpError::TokenExpired);
    }

    // Extract claims via mapping.
    let org_id = extract_string_claim(&claims, &config.claim_mapping.org_claim)?;
    let project_id = claims
        .get(&config.claim_mapping.project_claim)
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    let workload_id = extract_string_claim(&claims, &config.claim_mapping.workload_claim)?;

    Ok(ValidatedClaims {
        org_id,
        project_id,
        workload_id,
        issuer: issuer.into(),
        expires_at: exp,
    })
}

/// Verify an HS256 (HMAC-SHA256) JWT signature.
fn verify_hs256(
    header_b64: &str,
    payload_b64: &str,
    signature_b64: &str,
    secret: &str,
) -> Result<(), IdpError> {
    use aws_lc_rs::hmac;

    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let signing_input = format!("{header_b64}.{payload_b64}");

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|e| IdpError::InvalidToken(format!("signature base64 decode failed: {e}")))?;

    hmac::verify(&key, signing_input.as_bytes(), &sig_bytes)
        .map_err(|_| IdpError::InvalidToken("HS256 signature verification failed".into()))
}

/// Extract a required string claim from the JWT payload.
fn extract_string_claim(claims: &serde_json::Value, claim_name: &str) -> Result<String, IdpError> {
    claims
        .get(claim_name)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| IdpError::MissingClaim(claim_name.into()))
}

/// Build an HS256-signed JWT for testing purposes.
///
/// Creates a token with the given claims JSON as the payload,
/// signed with the provided secret using HMAC-SHA256.
#[cfg(test)]
fn build_test_jwt_hs256(claims: &serde_json::Value, secret: &str) -> String {
    use aws_lc_rs::hmac;

    let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signing_input = format!("{header}.{payload}");

    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(tag.as_ref());

    format!("{signing_input}.{signature}")
}

/// Build a JWT with a custom header for testing (e.g., alg=none).
#[cfg(test)]
fn build_test_jwt_with_header(
    header_json: &str,
    claims: &serde_json::Value,
    signature: &str,
) -> String {
    let header = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    format!("{header}.{payload}.{signature}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn future_exp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600
    }

    fn past_exp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 60
    }

    const TEST_SECRET: &str = "test-shared-secret-for-hs256";

    fn test_config() -> TenantIdpConfig {
        TenantIdpConfig {
            issuer_url: "https://idp.example.com".into(),
            audience: Some("kiseki-api".into()),
            claim_mapping: ClaimMapping::default(),
            unsafe_no_signature_verify: false,
            shared_secret: Some(TEST_SECRET.into()),
        }
    }

    fn valid_claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "https://idp.example.com",
            "aud": "kiseki-api",
            "exp": future_exp(),
            "sub": "workload-42",
            "org": "acme-corp",
            "project": "project-alpha"
        })
    }

    #[test]
    fn valid_claims_extracted() {
        let token = build_test_jwt_hs256(&valid_claims(), TEST_SECRET);
        let config = test_config();

        let result = validate_jwt(&token, &config).unwrap();

        assert_eq!(result.org_id, "acme-corp");
        assert_eq!(result.project_id.as_deref(), Some("project-alpha"));
        assert_eq!(result.workload_id, "workload-42");
        assert_eq!(result.issuer, "https://idp.example.com");
    }

    #[test]
    fn expired_token_rejected() {
        let mut claims = valid_claims();
        claims["exp"] = serde_json::json!(past_exp());
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config());

        assert!(matches!(result, Err(IdpError::TokenExpired)));
    }

    #[test]
    fn wrong_issuer_rejected() {
        let mut claims = valid_claims();
        claims["iss"] = serde_json::json!("https://evil.example.com");
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config());

        assert!(matches!(
            result,
            Err(IdpError::InvalidIssuer {
                expected: _,
                got: _
            })
        ));
    }

    #[test]
    fn missing_claim_detected() {
        let mut claims = valid_claims();
        // Remove the org claim.
        claims.as_object_mut().unwrap().remove("org");
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config());

        assert!(matches!(result, Err(IdpError::MissingClaim(ref c)) if c == "org"));
    }

    #[test]
    fn default_claim_mapping() {
        let mapping = ClaimMapping::default();
        assert_eq!(mapping.org_claim, "org");
        assert_eq!(mapping.project_claim, "project");
        assert_eq!(mapping.workload_claim, "sub");
    }

    #[test]
    fn invalid_audience_rejected() {
        let mut claims = valid_claims();
        claims["aud"] = serde_json::json!("wrong-audience");
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config());

        assert!(matches!(result, Err(IdpError::InvalidAudience)));
    }

    #[test]
    fn malformed_token_rejected() {
        let result = validate_jwt("not-a-jwt", &test_config());
        assert!(matches!(result, Err(IdpError::InvalidToken(_))));
    }

    #[test]
    fn optional_project_claim() {
        let mut claims = valid_claims();
        claims.as_object_mut().unwrap().remove("project");
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config()).unwrap();
        assert!(result.project_id.is_none());
    }

    #[test]
    fn rs256_rejected_without_unsafe_flag() {
        // RS256 tokens require unsafe_no_signature_verify since JWKS is not implemented.
        let token = build_test_jwt_with_header(
            r#"{"alg":"RS256","typ":"JWT"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let config = TenantIdpConfig {
            unsafe_no_signature_verify: false,
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("JWKS not yet implemented"))
        );
    }

    #[test]
    fn audience_as_array() {
        let mut claims = valid_claims();
        claims["aud"] = serde_json::json!(["other-api", "kiseki-api"]);
        let token = build_test_jwt_hs256(&claims, TEST_SECRET);

        let result = validate_jwt(&token, &test_config());
        assert!(result.is_ok());
    }

    #[test]
    fn alg_none_rejected() {
        let token =
            build_test_jwt_with_header(r#"{"alg":"none","typ":"JWT"}"#, &valid_claims(), "");
        let result = validate_jwt(&token, &test_config());
        assert!(matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("alg=none")));
    }

    #[test]
    fn hs256_correct_secret_passes() {
        let token = build_test_jwt_hs256(&valid_claims(), TEST_SECRET);
        let result = validate_jwt(&token, &test_config());
        assert!(result.is_ok());
    }

    #[test]
    fn hs256_wrong_secret_fails() {
        let token = build_test_jwt_hs256(&valid_claims(), "wrong-secret");
        let result = validate_jwt(&token, &test_config());
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("HS256 signature verification failed"))
        );
    }

    #[test]
    fn unsupported_algorithm_rejected() {
        let token = build_test_jwt_with_header(
            r#"{"alg":"PS256","typ":"JWT"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let result = validate_jwt(&token, &test_config());
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("unsupported algorithm"))
        );
    }

    #[test]
    fn hs256_no_shared_secret_configured() {
        let token = build_test_jwt_hs256(&valid_claims(), TEST_SECRET);
        let config = TenantIdpConfig {
            shared_secret: None,
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("no shared_secret configured"))
        );
    }
}
