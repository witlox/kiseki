//! OIDC/JWT validation for tenant identity (I-Auth2).
//!
//! Decodes and validates JWT tokens against per-tenant OIDC configuration.
//! Validates structure, issuer, expiry, and extracts claims via configurable
//! mapping. Supports HS256 (shared secret), RS256 (JWKS RSA), and ES256
//! (JWKS EC P-256) signature verification using `aws-lc-rs`.

use aws_lc_rs::signature;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// JSON Web Key Set — cached from the identity provider's JWKS endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Jwks {
    /// The set of JSON Web Keys.
    pub keys: Vec<Jwk>,
}

/// A single JSON Web Key (RFC 7517).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Jwk {
    /// Key type: `"RSA"` or `"EC"`.
    pub kty: String,
    /// Key ID (used to match the JWT header `kid`).
    #[serde(default)]
    pub kid: Option<String>,
    /// Algorithm: `"RS256"` or `"ES256"`.
    #[serde(default)]
    pub alg: Option<String>,
    /// RSA modulus (base64url-encoded, big-endian unsigned integer).
    #[serde(default)]
    pub n: Option<String>,
    /// RSA public exponent (base64url-encoded).
    #[serde(default)]
    pub e: Option<String>,
    /// EC x coordinate (base64url-encoded).
    #[serde(default)]
    pub x: Option<String>,
    /// EC y coordinate (base64url-encoded).
    #[serde(default)]
    pub y: Option<String>,
    /// EC curve name (e.g. `"P-256"`).
    #[serde(default)]
    pub crv: Option<String>,
}

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
    /// For RS256/ES256 tokens without `jwks_keys`, this flag must be set to
    /// `true` to bypass verification. When `jwks_keys` is provided, signatures
    /// are verified cryptographically regardless of this flag.
    #[serde(default)]
    pub unsafe_no_signature_verify: bool,
    /// Shared secret for HS256 signature verification.
    ///
    /// When set, tokens with `alg: HS256` are verified using HMAC-SHA256
    /// with this secret. Required for HS256 tokens.
    #[serde(default)]
    pub shared_secret: Option<String>,
    /// Pre-fetched JWKS keys for RS256/ES256 verification.
    ///
    /// When set, RS256 and ES256 tokens are verified against matching keys
    /// from this key set. Fetching keys from the identity provider's JWKS URL is deferred
    /// to a higher layer.
    #[serde(default)]
    pub jwks_keys: Option<Jwks>,
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
/// signature verification, issuer verification, expiry check, and
/// claim extraction.
///
/// Supported algorithms:
/// - **HS256**: verified via `shared_secret` (HMAC-SHA256).
/// - **RS256**: verified via JWKS keys (RSASSA-PKCS1-v1_5 with SHA-256).
/// - **ES256**: verified via JWKS keys (ECDSA P-256 with SHA-256).
///
/// For RS256/ES256, if `jwks_keys` is not configured, the token is rejected
/// unless `unsafe_no_signature_verify` is set.
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
    verify_jwt_signature(alg, &header, &parts, config)?;

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

/// Dispatch JWT signature verification by algorithm.
fn verify_jwt_signature(
    alg: &str,
    header: &serde_json::Value,
    parts: &[&str],
    config: &TenantIdpConfig,
) -> Result<(), IdpError> {
    match alg {
        "HS256" => {
            let secret = config.shared_secret.as_deref().ok_or_else(|| {
                IdpError::InvalidToken("HS256 token but no shared_secret configured".into())
            })?;
            verify_hs256(parts[0], parts[1], parts[2], secret)
        }
        "RS256" => verify_jwks_signature(header, parts, config, "RSA", "RS256"),
        "ES256" => verify_jwks_signature(header, parts, config, "EC", "ES256"),
        _ => unreachable!(), // Covered by the allowlist check in validate_jwt.
    }
}

/// Verify a JWKS-based (RS256/ES256) JWT signature.
fn verify_jwks_signature(
    header: &serde_json::Value,
    parts: &[&str],
    config: &TenantIdpConfig,
    kty: &str,
    alg: &str,
) -> Result<(), IdpError> {
    if let Some(ref jwks) = config.jwks_keys {
        let kid = header.get("kid").and_then(|v| v.as_str());
        let jwk = find_jwk(&jwks.keys, kid, kty)
            .ok_or_else(|| IdpError::InvalidToken(format!("no matching {kty} JWK for kid")))?;
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|e| IdpError::InvalidToken(format!("signature base64 decode failed: {e}")))?;
        match alg {
            "RS256" => verify_rs256(signing_input.as_bytes(), &sig_bytes, jwk),
            "ES256" => verify_es256(signing_input.as_bytes(), &sig_bytes, jwk),
            _ => unreachable!(),
        }
    } else if config.unsafe_no_signature_verify {
        tracing::warn!(
            "JWT {alg} signature verification skipped — \
             unsafe_no_signature_verify is set"
        );
        Ok(())
    } else {
        Err(IdpError::InvalidToken(format!(
            "{alg} requires jwks_keys or unsafe_no_signature_verify"
        )))
    }
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

/// Decode a base64url-encoded string (no padding).
fn base64url_decode(input: &str) -> Result<Vec<u8>, IdpError> {
    URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|e| IdpError::InvalidToken(format!("base64url decode failed: {e}")))
}

/// Find a matching JWK by key ID and key type.
///
/// If `kid` is `None`, returns the first key matching `kty`.
/// If `kid` is `Some`, returns the key matching both `kid` and `kty`.
fn find_jwk<'a>(keys: &'a [Jwk], kid: Option<&str>, kty: &str) -> Option<&'a Jwk> {
    keys.iter().find(|k| {
        k.kty == kty
            && match (kid, k.kid.as_deref()) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) => false,
                (None, _) => true,
            }
    })
}

/// Verify an RS256 (RSASSA-PKCS1-v1_5 SHA-256) signature using a JWK.
fn verify_rs256(signing_input: &[u8], sig_bytes: &[u8], jwk: &Jwk) -> Result<(), IdpError> {
    let n = base64url_decode(jwk.n.as_deref().unwrap_or(""))?;
    let e = base64url_decode(jwk.e.as_deref().unwrap_or(""))?;

    if n.is_empty() || e.is_empty() {
        return Err(IdpError::InvalidToken(
            "RSA JWK missing n or e component".into(),
        ));
    }

    let components = signature::RsaPublicKeyComponents { n: &n, e: &e };
    components
        .verify(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            signing_input,
            sig_bytes,
        )
        .map_err(|_| IdpError::InvalidToken("RS256 signature verification failed".into()))
}

/// Verify an ES256 (ECDSA P-256 SHA-256) signature using a JWK.
fn verify_es256(signing_input: &[u8], sig_bytes: &[u8], jwk: &Jwk) -> Result<(), IdpError> {
    let x = base64url_decode(jwk.x.as_deref().unwrap_or(""))?;
    let y = base64url_decode(jwk.y.as_deref().unwrap_or(""))?;

    if x.is_empty() || y.is_empty() {
        return Err(IdpError::InvalidToken(
            "EC JWK missing x or y coordinate".into(),
        ));
    }

    // Uncompressed EC point: 0x04 || x || y
    let mut point = Vec::with_capacity(1 + x.len() + y.len());
    point.push(0x04);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);

    let key = signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &point);
    key.verify(signing_input, sig_bytes)
        .map_err(|_| IdpError::InvalidToken("ES256 signature verification failed".into()))
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
            jwks_keys: None,
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
    fn rs256_without_jwks_requires_unsafe() {
        let token = build_test_jwt_with_header(
            r#"{"alg":"RS256","typ":"JWT"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let config = TenantIdpConfig {
            unsafe_no_signature_verify: false,
            jwks_keys: None,
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("RS256 requires jwks_keys"))
        );
    }

    #[test]
    fn es256_without_jwks_requires_unsafe() {
        let token = build_test_jwt_with_header(
            r#"{"alg":"ES256","typ":"JWT"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let config = TenantIdpConfig {
            unsafe_no_signature_verify: false,
            jwks_keys: None,
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("ES256 requires jwks_keys"))
        );
    }

    #[test]
    fn rs256_jwks_wrong_kid_rejected() {
        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "RSA".into(),
                kid: Some("key-1".into()),
                alg: Some("RS256".into()),
                n: Some("dGVzdA".into()),
                e: Some("AQAB".into()),
                x: None,
                y: None,
                crv: None,
            }],
        };
        // Token header has kid=key-999 which doesn't match key-1.
        let token = build_test_jwt_with_header(
            r#"{"alg":"RS256","typ":"JWT","kid":"key-999"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let config = TenantIdpConfig {
            jwks_keys: Some(jwks),
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("no matching RSA JWK"))
        );
    }

    #[test]
    fn es256_jwks_wrong_kid_rejected() {
        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "EC".into(),
                kid: Some("ec-key-1".into()),
                alg: Some("ES256".into()),
                n: None,
                e: None,
                x: Some("dGVzdA".into()),
                y: Some("dGVzdA".into()),
                crv: Some("P-256".into()),
            }],
        };
        let token = build_test_jwt_with_header(
            r#"{"alg":"ES256","typ":"JWT","kid":"ec-key-999"}"#,
            &valid_claims(),
            "fake-sig",
        );
        let config = TenantIdpConfig {
            jwks_keys: Some(jwks),
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("no matching EC JWK"))
        );
    }

    #[test]
    fn jwks_deserialize() {
        let json = r#"{
            "keys": [
                {
                    "kty": "RSA",
                    "kid": "rsa-key-1",
                    "alg": "RS256",
                    "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw",
                    "e": "AQAB"
                },
                {
                    "kty": "EC",
                    "kid": "ec-key-1",
                    "alg": "ES256",
                    "crv": "P-256",
                    "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
                    "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
                }
            ]
        }"#;
        let jwks: Jwks = serde_json::from_str(json).unwrap();
        assert_eq!(jwks.keys.len(), 2);
        assert_eq!(jwks.keys[0].kty, "RSA");
        assert_eq!(jwks.keys[0].kid.as_deref(), Some("rsa-key-1"));
        assert!(jwks.keys[0].n.is_some());
        assert_eq!(jwks.keys[1].kty, "EC");
        assert_eq!(jwks.keys[1].crv.as_deref(), Some("P-256"));
    }

    #[test]
    fn base64url_decode_works() {
        // "AQAB" is base64url for [1, 0, 1] (RSA exponent 65537)
        let decoded = base64url_decode("AQAB").unwrap();
        assert_eq!(decoded, vec![1, 0, 1]);

        // Empty input decodes to empty vec.
        let empty = base64url_decode("").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn es256_verify_with_generated_key() {
        use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

        // Generate an EC P-256 key pair.
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();

        // Extract the public key (uncompressed point: 0x04 || x || y).
        let pub_key_bytes = key_pair.public_key().as_ref();
        assert_eq!(pub_key_bytes.len(), 65); // 1 + 32 + 32
        assert_eq!(pub_key_bytes[0], 0x04);
        let x_bytes = &pub_key_bytes[1..33];
        let y_bytes = &pub_key_bytes[33..65];

        let x_b64 = URL_SAFE_NO_PAD.encode(x_bytes);
        let y_b64 = URL_SAFE_NO_PAD.encode(y_bytes);

        // Build the JWKS.
        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "EC".into(),
                kid: Some("test-ec".into()),
                alg: Some("ES256".into()),
                n: None,
                e: None,
                x: Some(x_b64),
                y: Some(y_b64),
                crv: Some("P-256".into()),
            }],
        };

        // Build the JWT.
        let header_json = r#"{"alg":"ES256","typ":"JWT","kid":"test-ec"}"#;
        let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
        let payload_b64 = URL_SAFE_NO_PAD.encode(valid_claims().to_string().as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");

        // Sign the token.
        let sig = key_pair.sign(&rng, signing_input.as_bytes()).unwrap();
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());
        let token = format!("{signing_input}.{sig_b64}");

        let config = TenantIdpConfig {
            jwks_keys: Some(jwks),
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(result.is_ok(), "ES256 verification failed: {result:?}");
    }

    #[test]
    fn es256_wrong_signature_rejected() {
        use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

        // Generate two different key pairs.
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8_sign =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let sign_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8_sign.as_ref())
                .unwrap();

        let pkcs8_verify =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let verify_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8_verify.as_ref())
                .unwrap();

        // Use the verification pair's public key in JWKS but sign with the other key.
        let pub_key_bytes = verify_pair.public_key().as_ref();
        let x_b64 = URL_SAFE_NO_PAD.encode(&pub_key_bytes[1..33]);
        let y_b64 = URL_SAFE_NO_PAD.encode(&pub_key_bytes[33..65]);

        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "EC".into(),
                kid: None,
                alg: Some("ES256".into()),
                n: None,
                e: None,
                x: Some(x_b64),
                y: Some(y_b64),
                crv: Some("P-256".into()),
            }],
        };

        let header_json = r#"{"alg":"ES256","typ":"JWT"}"#;
        let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
        let payload_b64 = URL_SAFE_NO_PAD.encode(valid_claims().to_string().as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");

        // Sign with the WRONG key.
        let sig = sign_pair.sign(&rng, signing_input.as_bytes()).unwrap();
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());
        let token = format!("{signing_input}.{sig_b64}");

        let config = TenantIdpConfig {
            jwks_keys: Some(jwks),
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(
            matches!(result, Err(IdpError::InvalidToken(ref msg)) if msg.contains("ES256 signature verification failed"))
        );
    }

    /// Parse an RFC 8017 `RSAPublicKey` DER: `SEQUENCE { INTEGER n, INTEGER e }`.
    /// Returns (n, e) as big-endian unsigned byte slices (leading zeros stripped).
    fn parse_rsa_public_key_der(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // Minimal DER parser: we just need two INTEGERs from a SEQUENCE.
        let mut pos = 0;

        // Outer SEQUENCE tag (0x30).
        assert_eq!(der[pos], 0x30, "expected SEQUENCE");
        pos += 1;
        let (_seq_len, consumed) = parse_der_length(&der[pos..]);
        pos += consumed;

        // First INTEGER (n).
        assert_eq!(der[pos], 0x02, "expected INTEGER for n");
        pos += 1;
        let (n_len, consumed) = parse_der_length(&der[pos..]);
        pos += consumed;
        let mut n = der[pos..pos + n_len].to_vec();
        pos += n_len;
        // Strip leading zero (DER sign byte).
        if n.first() == Some(&0) && n.len() > 1 {
            n.remove(0);
        }

        // Second INTEGER (e).
        assert_eq!(der[pos], 0x02, "expected INTEGER for e");
        pos += 1;
        let (e_len, consumed) = parse_der_length(&der[pos..]);
        pos += consumed;
        let mut e = der[pos..pos + e_len].to_vec();
        if e.first() == Some(&0) && e.len() > 1 {
            e.remove(0);
        }

        (n, e)
    }

    /// Parse a DER length field, returning (length, bytes consumed).
    fn parse_der_length(data: &[u8]) -> (usize, usize) {
        if data[0] < 0x80 {
            (data[0] as usize, 1)
        } else {
            let num_bytes = (data[0] & 0x7F) as usize;
            let mut len = 0usize;
            for byte in &data[1..=num_bytes] {
                len = (len << 8) | (*byte as usize);
            }
            (len, 1 + num_bytes)
        }
    }

    #[test]
    fn rs256_verify_with_generated_key() {
        use aws_lc_rs::rsa;
        use aws_lc_rs::signature::KeyPair;

        // Generate an RSA 2048-bit key pair.
        let key_pair = rsa::KeyPair::generate(rsa::KeySize::Rsa2048).unwrap();

        // Extract n and e from the DER-encoded public key (RFC 8017 format).
        let pub_key_der = key_pair.public_key().as_ref();
        let (n_bytes, e_bytes) = parse_rsa_public_key_der(pub_key_der);

        let n_b64 = URL_SAFE_NO_PAD.encode(&n_bytes);
        let e_b64 = URL_SAFE_NO_PAD.encode(&e_bytes);

        let jwks = Jwks {
            keys: vec![Jwk {
                kty: "RSA".into(),
                kid: Some("test-rsa".into()),
                alg: Some("RS256".into()),
                n: Some(n_b64),
                e: Some(e_b64),
                x: None,
                y: None,
                crv: None,
            }],
        };

        let header_json = r#"{"alg":"RS256","typ":"JWT","kid":"test-rsa"}"#;
        let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
        let payload_b64 = URL_SAFE_NO_PAD.encode(valid_claims().to_string().as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");

        // Sign the token with PKCS1 SHA-256.
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let mut sig_buf = vec![0u8; key_pair.public_modulus_len()];
        key_pair
            .sign(
                &signature::RSA_PKCS1_SHA256,
                &rng,
                signing_input.as_bytes(),
                &mut sig_buf,
            )
            .unwrap();
        let sig_b64 = URL_SAFE_NO_PAD.encode(&sig_buf);
        let token = format!("{signing_input}.{sig_b64}");

        let config = TenantIdpConfig {
            jwks_keys: Some(jwks),
            ..test_config()
        };
        let result = validate_jwt(&token, &config);
        assert!(result.is_ok(), "RS256 verification failed: {result:?}");
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

    // ---------------------------------------------------------------
    // Scenario: Tenant with IdP configured — second-stage validation
    // Valid mTLS + valid workload token => accepted with full identity.
    // ---------------------------------------------------------------
    #[test]
    fn idp_configured_valid_token_accepted() {
        let config = test_config();
        let token = build_test_jwt_hs256(&valid_claims(), TEST_SECRET);

        let result = validate_jwt(&token, &config).unwrap();
        assert_eq!(result.org_id, "acme-corp");
        assert_eq!(result.workload_id, "workload-42");
        assert_eq!(result.issuer, "https://idp.example.com");
        // Full workload identity: org + workload extracted.
        assert!(result.project_id.is_some());
    }

    // ---------------------------------------------------------------
    // Scenario: Tenant with IdP configured — missing token
    // When IdP is required but no token is presented, reject.
    // ---------------------------------------------------------------
    #[test]
    fn idp_configured_missing_token_rejected() {
        let config = test_config();
        // An empty string is not a valid JWT — simulates "no token".
        let result = validate_jwt("", &config);
        assert!(result.is_err());
        assert!(matches!(result, Err(IdpError::InvalidToken(_))));
    }

    // ---------------------------------------------------------------
    // Scenario: Tenant without IdP — mTLS only (sufficient)
    // When no IdP is configured, there is nothing to validate.
    // The absence of config means mTLS alone is sufficient.
    // ---------------------------------------------------------------
    #[test]
    fn no_idp_config_mtls_only_sufficient() {
        // With no IdP configured, there is no token to validate —
        // the caller simply skips the IdP validation step.
        // Verify that the config can represent "no IdP" state.
        let no_idp = TenantIdpConfig {
            issuer_url: String::new(),
            audience: None,
            claim_mapping: ClaimMapping::default(),
            unsafe_no_signature_verify: false,
            shared_secret: None,
            jwks_keys: None,
        };
        // An empty issuer_url indicates no IdP is configured.
        assert!(no_idp.issuer_url.is_empty());
        assert!(no_idp.shared_secret.is_none());
        assert!(no_idp.jwks_keys.is_none());
    }
}
