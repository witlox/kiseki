//! `HashiCorp` Vault Transit provider (feature: `kms-vault`).
//!
//! Wraps/unwraps DEK material via the Vault Transit secrets engine.
//! Uses raw `TcpStream` HTTP (blocking) — no reqwest dependency.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::prelude::*;

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Configuration for Vault Transit KMS provider.
#[derive(Clone, Debug)]
pub struct VaultConfig {
    /// Vault API endpoint, e.g. `"http://vault:8200"`.
    pub endpoint: String,
    /// Vault authentication token.
    pub token: String,
    /// Transit secret-engine key name.
    pub transit_key: String,
    /// Vault Enterprise namespace (optional).
    pub namespace: Option<String>,
    /// PEM-encoded CA certificate for mTLS to Vault (optional).
    pub tls_ca_pem: Option<Vec<u8>>,
}

/// Vault Transit KMS provider.
///
/// Performs wrap/unwrap via Vault's Transit encrypt/decrypt API
/// using blocking TCP HTTP requests.
#[derive(Debug)]
pub struct VaultProvider {
    config: VaultConfig,
    epoch: AtomicU64,
}

/// Parsed host and port from a Vault endpoint URL.
#[derive(Debug)]
struct HostPort {
    host: String,
    port: u16,
}

/// Parse `http://host:port` into components. Only HTTP is supported
/// for the raw TCP path; HTTPS would require TLS (use a real client).
fn parse_endpoint(endpoint: &str) -> Result<HostPort, KmsError> {
    let stripped = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| KmsError::Unavailable("vault endpoint must start with http://".into()))?;
    let (host, port) = if let Some((h, p)) = stripped.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .map_err(|_| KmsError::Unavailable(format!("invalid port in endpoint: {p}")))?;
        (h.to_owned(), port)
    } else {
        (stripped.to_owned(), 8200)
    };
    Ok(HostPort { host, port })
}

/// Send an HTTP request to Vault and return the response body.
fn vault_request(
    method: &str,
    url: &str,
    token: &str,
    namespace: Option<&str>,
    body: Option<&str>,
) -> Result<String, KmsError> {
    // Parse the full URL to extract host:port and path.
    let after_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| KmsError::Unavailable("vault URL must start with http://".into()))?;
    let (host_port_str, path) = after_scheme
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((after_scheme, "/".to_owned()));

    let hp = parse_endpoint(&format!("http://{host_port_str}"))?;
    let addr = format!("{}:{}", hp.host, hp.port);

    let mut stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .map_err(|e| KmsError::Unavailable(format!("invalid address {addr}: {e}")))?,
        Duration::from_secs(5),
    )
    .map_err(|e| KmsError::Unavailable(format!("vault connect failed: {e}")))?;

    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let body_bytes = body.unwrap_or("");
    let content_length = body_bytes.len();

    let mut request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {host_port_str}\r\n\
         X-Vault-Token: {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {content_length}\r\n\
         Connection: close\r\n"
    );
    if let Some(ns) = namespace {
        use std::fmt::Write as _;
        let _ = write!(request, "X-Vault-Namespace: {ns}\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body_bytes);

    stream
        .write_all(request.as_bytes())
        .map_err(|e| KmsError::Unavailable(format!("vault write failed: {e}")))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| KmsError::Unavailable(format!("vault read failed: {e}")))?;

    let response_str = String::from_utf8_lossy(&response);

    // Parse HTTP status line.
    let status_line = response_str
        .lines()
        .next()
        .ok_or_else(|| KmsError::Unavailable("empty response from vault".into()))?;
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if !(200..300).contains(&status_code) {
        return Err(KmsError::Unavailable(format!(
            "vault returned HTTP {status_code}"
        )));
    }

    // Extract body after the blank line separating headers from body.
    let body_str = response_str
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();

    Ok(body_str)
}

/// Extract a string value from a simple JSON object by key path
/// `data.{field}`. This avoids pulling in a JSON parser for the
/// small Vault response payloads.
fn extract_json_field(json: &str, field: &str) -> Option<String> {
    // Look for `"<field>":"<value>"` or `"<field>": "<value>"`.
    let pattern = format!("\"{field}\"");
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    // Skip optional whitespace and colon.
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_ws = after_colon.trim_start();
    if let Some(value_start) = after_ws.strip_prefix('"') {
        let end = value_start.find('"')?;
        Some(value_start[..end].to_owned())
    } else {
        // Numeric or other non-string value.
        let end = after_ws.find(|c: char| c == ',' || c == '}' || c.is_whitespace())?;
        Some(after_ws[..end].to_owned())
    }
}

impl VaultProvider {
    /// Create a new Vault Transit provider from the given configuration.
    #[must_use]
    pub fn new(config: VaultConfig) -> Self {
        Self {
            config,
            epoch: AtomicU64::new(1),
        }
    }

    /// Return a reference to the provider configuration.
    #[must_use]
    pub fn config(&self) -> &VaultConfig {
        &self.config
    }

    /// Build a Vault Transit encrypt request body.
    fn build_encrypt_body(plaintext: &[u8], aad: &[u8]) -> String {
        let pt_b64 = BASE64_STANDARD.encode(plaintext);
        let ctx_b64 = BASE64_STANDARD.encode(aad);
        format!(r#"{{"plaintext":"{pt_b64}","context":"{ctx_b64}"}}"#)
    }

    /// Build a Vault Transit decrypt request body.
    fn build_decrypt_body(ciphertext_token: &str, aad: &[u8]) -> String {
        let ctx_b64 = BASE64_STANDARD.encode(aad);
        format!(r#"{{"ciphertext":"{ciphertext_token}","context":"{ctx_b64}"}}"#)
    }
}

impl TenantKmsProvider for VaultProvider {
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let url = format!(
            "{}/v1/transit/encrypt/{}",
            self.config.endpoint, self.config.transit_key
        );
        let body = Self::build_encrypt_body(plaintext, aad);
        let response = vault_request(
            "POST",
            &url,
            &self.config.token,
            self.config.namespace.as_deref(),
            Some(&body),
        )?;

        // Response: {"data":{"ciphertext":"vault:v1:..."}}
        let ct = extract_json_field(&response, "ciphertext")
            .ok_or_else(|| KmsError::CryptoError("missing ciphertext in vault response".into()))?;

        Ok(ct.into_bytes())
    }

    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let ct_str = std::str::from_utf8(ciphertext)
            .map_err(|_| KmsError::CryptoError("ciphertext is not valid UTF-8".into()))?;

        let url = format!(
            "{}/v1/transit/decrypt/{}",
            self.config.endpoint, self.config.transit_key
        );
        let body = Self::build_decrypt_body(ct_str, aad);
        let response = vault_request(
            "POST",
            &url,
            &self.config.token,
            self.config.namespace.as_deref(),
            Some(&body),
        )?;

        // Response: {"data":{"plaintext":"base64..."}}
        let pt_b64 = extract_json_field(&response, "plaintext")
            .ok_or_else(|| KmsError::CryptoError("missing plaintext in vault response".into()))?;

        BASE64_STANDARD
            .decode(pt_b64)
            .map_err(|e| KmsError::CryptoError(format!("base64 decode failed: {e}")))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        let url = format!(
            "{}/v1/transit/keys/{}/rotate",
            self.config.endpoint, self.config.transit_key
        );
        vault_request(
            "POST",
            &url,
            &self.config.token,
            self.config.namespace.as_deref(),
            None,
        )?;

        let new_epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(format!("vault-epoch-{new_epoch}"))
    }

    fn health_check(&self) -> KmsHealth {
        let url = format!("{}/v1/sys/health", self.config.endpoint);
        match vault_request("GET", &url, &self.config.token, None, None) {
            Ok(_) => KmsHealth::Healthy,
            Err(e) => KmsHealth::Unavailable(format!("{e}")),
        }
    }

    fn name(&self) -> &'static str {
        "vault-transit"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> VaultConfig {
        VaultConfig {
            endpoint: "http://vault.test:8200".into(),
            token: "s.test-token".into(),
            transit_key: "kiseki-tenant-key".into(),
            namespace: Some("admin".into()),
            tls_ca_pem: None,
        }
    }

    #[test]
    fn config_construction() {
        let cfg = test_config();
        let provider = VaultProvider::new(cfg);
        assert_eq!(provider.config().endpoint, "http://vault.test:8200");
        assert_eq!(provider.config().transit_key, "kiseki-tenant-key");
        assert_eq!(provider.config().namespace.as_deref(), Some("admin"));
    }

    #[test]
    fn name_is_vault_transit() {
        let provider = VaultProvider::new(test_config());
        assert_eq!(provider.name(), "vault-transit");
    }

    #[test]
    fn encrypt_request_body_format() {
        let body = VaultProvider::build_encrypt_body(b"secret-dek", b"tenant-1:chunk-42");
        // Verify it contains valid base64 encodings.
        assert!(body.contains("\"plaintext\":\""));
        assert!(body.contains("\"context\":\""));

        // Parse the base64 values back.
        let pt_b64 = extract_json_field(&body, "plaintext").unwrap();
        let ctx_b64 = extract_json_field(&body, "context").unwrap();
        let pt = BASE64_STANDARD.decode(pt_b64).unwrap();
        let ctx = BASE64_STANDARD.decode(ctx_b64).unwrap();
        assert_eq!(pt, b"secret-dek");
        assert_eq!(ctx, b"tenant-1:chunk-42");
    }

    #[test]
    fn decrypt_request_body_format() {
        let body =
            VaultProvider::build_decrypt_body("vault:v1:abc123ciphertext", b"tenant-1:chunk-42");
        assert!(body.contains("\"ciphertext\":\"vault:v1:abc123ciphertext\""));
        assert!(body.contains("\"context\":\""));

        let ctx_b64 = extract_json_field(&body, "context").unwrap();
        let ctx = BASE64_STANDARD.decode(ctx_b64).unwrap();
        assert_eq!(ctx, b"tenant-1:chunk-42");
    }

    #[test]
    fn extract_json_field_works() {
        let json = r#"{"data":{"ciphertext":"vault:v1:xyz","plaintext":"SGVsbG8="}}"#;
        assert_eq!(
            extract_json_field(json, "ciphertext").unwrap(),
            "vault:v1:xyz"
        );
        assert_eq!(extract_json_field(json, "plaintext").unwrap(), "SGVsbG8=");
        assert!(extract_json_field(json, "missing").is_none());
    }

    #[test]
    fn parse_endpoint_valid() {
        let hp = parse_endpoint("http://vault:8200").unwrap();
        assert_eq!(hp.host, "vault");
        assert_eq!(hp.port, 8200);
    }

    #[test]
    fn parse_endpoint_default_port() {
        let hp = parse_endpoint("http://vault.prod").unwrap();
        assert_eq!(hp.host, "vault.prod");
        assert_eq!(hp.port, 8200);
    }

    #[test]
    fn parse_endpoint_https_rejected() {
        let err = parse_endpoint("https://vault:8200").unwrap_err();
        assert!(matches!(err, KmsError::Unavailable(_)));
    }
}
