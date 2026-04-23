//! Azure Key Vault provider (feature: `kms-azure`).
//!
//! Request construction targets the Azure Key Vault REST API.
//! HTTP transport returns [`KmsError::Unavailable`] until a real
//! HTTP client (reqwest or similar) is linked.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::prelude::*;

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Configuration for Azure Key Vault provider.
#[derive(Clone, Debug)]
pub struct AzureKvConfig {
    /// Vault URL, e.g. `"https://myvault.vault.azure.net"`.
    pub vault_url: String,
    /// Key name in the vault.
    pub key_name: String,
    /// Key version (empty string for latest).
    pub key_version: String,
    /// Azure AD tenant ID.
    pub tenant_id: String,
    /// Service principal client ID.
    pub client_id: String,
    /// Service principal client secret.
    pub client_secret: String,
}

/// Azure Key Vault KMS provider.
///
/// Request construction is functional; HTTP transport returns
/// `Unavailable` until a real HTTP client is linked.
#[derive(Debug)]
pub struct AzureKvProvider {
    config: AzureKvConfig,
    epoch: AtomicU64,
}

impl AzureKvProvider {
    /// Create a new Azure Key Vault provider from the given configuration.
    #[must_use]
    pub fn new(config: AzureKvConfig) -> Self {
        Self {
            config,
            epoch: AtomicU64::new(1),
        }
    }

    /// Return a reference to the provider configuration.
    #[must_use]
    pub fn config(&self) -> &AzureKvConfig {
        &self.config
    }

    /// Build the encrypt URL for Azure Key Vault.
    #[must_use]
    pub fn encrypt_url(&self) -> String {
        let version_segment = if self.config.key_version.is_empty() {
            String::new()
        } else {
            format!("/{}", self.config.key_version)
        };
        format!(
            "{}/keys/{}{}/encrypt?api-version=7.4",
            self.config.vault_url, self.config.key_name, version_segment
        )
    }

    /// Build the decrypt URL for Azure Key Vault.
    #[must_use]
    pub fn decrypt_url(&self) -> String {
        let version_segment = if self.config.key_version.is_empty() {
            String::new()
        } else {
            format!("/{}", self.config.key_version)
        };
        format!(
            "{}/keys/{}{}/decrypt?api-version=7.4",
            self.config.vault_url, self.config.key_name, version_segment
        )
    }

    /// Build the create-new-version URL for key rotation.
    #[must_use]
    pub fn rotate_url(&self) -> String {
        format!(
            "{}/keys/{}/create?api-version=7.4",
            self.config.vault_url, self.config.key_name
        )
    }

    /// Build the key info URL for health checking.
    #[must_use]
    pub fn key_url(&self) -> String {
        format!(
            "{}/keys/{}?api-version=7.4",
            self.config.vault_url, self.config.key_name
        )
    }

    /// Build the JSON body for an Azure encrypt request.
    #[must_use]
    pub fn build_encrypt_body(plaintext: &[u8]) -> String {
        let value_b64url = BASE64_URL_SAFE_NO_PAD.encode(plaintext);
        format!(r#"{{"alg":"RSA-OAEP-256","value":"{value_b64url}"}}"#)
    }

    /// Build the JSON body for an Azure decrypt request.
    #[must_use]
    pub fn build_decrypt_body(ciphertext: &[u8]) -> String {
        let value_b64url = BASE64_URL_SAFE_NO_PAD.encode(ciphertext);
        format!(r#"{{"alg":"RSA-OAEP-256","value":"{value_b64url}"}}"#)
    }

    /// The Azure AD token endpoint for this tenant.
    #[must_use]
    pub fn token_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.config.tenant_id
        )
    }
}

impl TenantKmsProvider for AzureKvProvider {
    fn wrap(&self, plaintext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let _url = self.encrypt_url();
        let _body = Self::build_encrypt_body(plaintext);
        Err(KmsError::Unavailable(
            "azure HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn unwrap(&self, ciphertext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let _url = self.decrypt_url();
        let _body = Self::build_decrypt_body(ciphertext);
        Err(KmsError::Unavailable(
            "azure HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        let _ = self.epoch.load(Ordering::Relaxed);
        let _url = self.rotate_url();
        Err(KmsError::Unavailable(
            "azure HTTP client not linked — requires OAuth2 bearer token".into(),
        ))
    }

    fn health_check(&self) -> KmsHealth {
        let _ = self.epoch.load(Ordering::Relaxed);
        KmsHealth::Unavailable("azure HTTP client not linked — requires OAuth2 bearer token".into())
    }

    fn name(&self) -> &'static str {
        "azure-keyvault"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AzureKvConfig {
        AzureKvConfig {
            vault_url: "https://myvault.vault.azure.net".into(),
            key_name: "kiseki-tenant-key".into(),
            key_version: "abc123".into(),
            tenant_id: "00000000-0000-0000-0000-000000000001".into(),
            client_id: "00000000-0000-0000-0000-000000000002".into(),
            client_secret: "test-secret-value".into(),
        }
    }

    #[test]
    fn config_construction() {
        let cfg = test_config();
        let provider = AzureKvProvider::new(cfg);
        assert_eq!(
            provider.config().vault_url,
            "https://myvault.vault.azure.net"
        );
        assert_eq!(provider.config().key_name, "kiseki-tenant-key");
        assert_eq!(provider.config().key_version, "abc123");
        assert!(provider
            .encrypt_url()
            .contains("/keys/kiseki-tenant-key/abc123/encrypt"));
        assert!(provider
            .decrypt_url()
            .contains("/keys/kiseki-tenant-key/abc123/decrypt"));
        assert!(provider
            .rotate_url()
            .contains("/keys/kiseki-tenant-key/create"));
        assert!(provider
            .token_endpoint()
            .contains("00000000-0000-0000-0000-000000000001"));
    }

    #[test]
    fn name_is_azure_keyvault() {
        let provider = AzureKvProvider::new(test_config());
        assert_eq!(provider.name(), "azure-keyvault");
    }

    #[test]
    fn encrypt_body_uses_base64url() {
        let body = AzureKvProvider::build_encrypt_body(b"secret-dek");
        assert!(body.contains("\"alg\":\"RSA-OAEP-256\""));
        assert!(body.contains("\"value\":\""));
        // Verify base64url encoding roundtrips.
        let value = body
            .split("\"value\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let decoded = BASE64_URL_SAFE_NO_PAD.decode(value).unwrap();
        assert_eq!(decoded, b"secret-dek");
    }

    #[test]
    fn latest_version_url_omits_version_segment() {
        let mut cfg = test_config();
        cfg.key_version = String::new();
        let provider = AzureKvProvider::new(cfg);
        assert!(provider
            .encrypt_url()
            .contains("/keys/kiseki-tenant-key/encrypt"));
        // No double slash.
        assert!(!provider.encrypt_url().contains("//encrypt"));
    }
}
