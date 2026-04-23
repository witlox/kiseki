//! GCP Cloud KMS provider (feature: `kms-gcp`).
//!
//! Request construction targets the Cloud KMS REST API.
//! HTTP transport returns [`KmsError::Unavailable`] until a real
//! HTTP client and `OAuth2` token exchange are linked.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::prelude::*;

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Configuration for GCP Cloud KMS provider.
#[derive(Clone, Debug)]
pub struct GcpKmsConfig {
    /// GCP project ID.
    pub project_id: String,
    /// Location, e.g. `"global"` or `"europe-west6"`.
    pub location: String,
    /// Key ring name.
    pub key_ring: String,
    /// Crypto key name.
    pub key_name: String,
    /// Service account credentials JSON (optional; falls back to
    /// application default credentials).
    pub credentials_json: Option<String>,
}

/// GCP Cloud KMS provider.
///
/// Request construction is functional; HTTP transport returns
/// `Unavailable` until `OAuth2` and HTTP client are linked.
#[derive(Debug)]
pub struct GcpKmsProvider {
    config: GcpKmsConfig,
    epoch: AtomicU64,
}

impl GcpKmsProvider {
    /// Create a new GCP Cloud KMS provider from the given configuration.
    #[must_use]
    pub fn new(config: GcpKmsConfig) -> Self {
        Self {
            config,
            epoch: AtomicU64::new(1),
        }
    }

    /// Return a reference to the provider configuration.
    #[must_use]
    pub fn config(&self) -> &GcpKmsConfig {
        &self.config
    }

    /// The Cloud KMS resource name for this crypto key.
    #[must_use]
    pub fn resource_name(&self) -> String {
        format!(
            "projects/{}/locations/{}/keyRings/{}/cryptoKeys/{}",
            self.config.project_id,
            self.config.location,
            self.config.key_ring,
            self.config.key_name
        )
    }

    /// Build the encrypt URL.
    #[must_use]
    pub fn encrypt_url(&self) -> String {
        format!(
            "https://cloudkms.googleapis.com/v1/{}:encrypt",
            self.resource_name()
        )
    }

    /// Build the decrypt URL.
    #[must_use]
    pub fn decrypt_url(&self) -> String {
        format!(
            "https://cloudkms.googleapis.com/v1/{}:decrypt",
            self.resource_name()
        )
    }

    /// Build the URL to create a new crypto key version (rotation).
    #[must_use]
    pub fn create_version_url(&self) -> String {
        format!(
            "https://cloudkms.googleapis.com/v1/{}/cryptoKeyVersions",
            self.resource_name()
        )
    }

    /// Build the URL to GET key metadata (health check).
    #[must_use]
    pub fn key_metadata_url(&self) -> String {
        format!(
            "https://cloudkms.googleapis.com/v1/{}",
            self.resource_name()
        )
    }

    /// Build the JSON body for a Cloud KMS encrypt request.
    #[must_use]
    pub fn build_encrypt_body(plaintext: &[u8], aad: &[u8]) -> String {
        let pt_b64 = BASE64_STANDARD.encode(plaintext);
        let aad_b64 = BASE64_STANDARD.encode(aad);
        format!(r#"{{"plaintext":"{pt_b64}","additionalAuthenticatedData":"{aad_b64}"}}"#)
    }

    /// Build the JSON body for a Cloud KMS decrypt request.
    #[must_use]
    pub fn build_decrypt_body(ciphertext: &[u8], aad: &[u8]) -> String {
        let ct_b64 = BASE64_STANDARD.encode(ciphertext);
        let aad_b64 = BASE64_STANDARD.encode(aad);
        format!(r#"{{"ciphertext":"{ct_b64}","additionalAuthenticatedData":"{aad_b64}"}}"#)
    }
}

impl TenantKmsProvider for GcpKmsProvider {
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let _url = self.encrypt_url();
        let _body = Self::build_encrypt_body(plaintext, aad);
        Err(KmsError::Unavailable(
            "gcp HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let _url = self.decrypt_url();
        let _body = Self::build_decrypt_body(ciphertext, aad);
        Err(KmsError::Unavailable(
            "gcp HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        let _ = self.epoch.load(Ordering::Relaxed);
        let _url = self.create_version_url();
        Err(KmsError::Unavailable(
            "gcp HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn health_check(&self) -> KmsHealth {
        let _ = self.epoch.load(Ordering::Relaxed);
        KmsHealth::Unavailable("gcp HTTP client not linked — requires OAuth2 bearer token".into())
    }

    fn name(&self) -> &'static str {
        "gcp-cloudkms"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GcpKmsConfig {
        GcpKmsConfig {
            project_id: "my-project".into(),
            location: "europe-west6".into(),
            key_ring: "kiseki-ring".into(),
            key_name: "tenant-key".into(),
            credentials_json: None,
        }
    }

    #[test]
    fn config_construction() {
        let cfg = test_config();
        let provider = GcpKmsProvider::new(cfg);
        assert_eq!(provider.config().project_id, "my-project");
        assert_eq!(provider.config().location, "europe-west6");
        assert_eq!(provider.config().key_ring, "kiseki-ring");
        assert_eq!(provider.config().key_name, "tenant-key");
        assert_eq!(
            provider.resource_name(),
            "projects/my-project/locations/europe-west6/keyRings/kiseki-ring/cryptoKeys/tenant-key"
        );
        assert!(provider.encrypt_url().ends_with(":encrypt"));
        assert!(provider.decrypt_url().ends_with(":decrypt"));
        assert!(provider
            .create_version_url()
            .ends_with("/cryptoKeyVersions"));
    }

    #[test]
    fn name_is_gcp_cloudkms() {
        let provider = GcpKmsProvider::new(test_config());
        assert_eq!(provider.name(), "gcp-cloudkms");
    }

    #[test]
    fn encrypt_body_format() {
        let body = GcpKmsProvider::build_encrypt_body(b"secret-dek", b"tenant-1:chunk-42");
        assert!(body.contains("\"plaintext\":\""));
        assert!(body.contains("\"additionalAuthenticatedData\":\""));

        // Verify base64 roundtrips.
        let pt_b64 = body
            .split("\"plaintext\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let decoded = BASE64_STANDARD.decode(pt_b64).unwrap();
        assert_eq!(decoded, b"secret-dek");
    }

    #[test]
    fn decrypt_body_format() {
        let body = GcpKmsProvider::build_decrypt_body(b"encrypted-blob", b"aad-data");
        assert!(body.contains("\"ciphertext\":\""));
        assert!(body.contains("\"additionalAuthenticatedData\":\""));
    }

    #[test]
    fn stub_returns_unavailable() {
        let provider = GcpKmsProvider::new(test_config());

        let wrap_err = provider.wrap(b"plaintext", b"aad").unwrap_err();
        assert!(matches!(wrap_err, KmsError::Unavailable(_)));

        let unwrap_err = provider.unwrap(b"ciphertext", b"aad").unwrap_err();
        assert!(matches!(unwrap_err, KmsError::Unavailable(_)));

        let rotate_err = provider.rotate().unwrap_err();
        assert!(matches!(rotate_err, KmsError::Unavailable(_)));

        assert!(matches!(provider.health_check(), KmsHealth::Unavailable(_)));
    }
}
