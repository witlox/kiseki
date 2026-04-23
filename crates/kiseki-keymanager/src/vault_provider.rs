//! `HashiCorp` Vault Transit provider (feature: `kms-vault`).
//!
//! Compile-time stub that defines the types and API surface without
//! making HTTP calls. Activating the `kms-vault` feature is a
//! prerequisite for a future `reqwest`-backed implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Configuration for Vault Transit KMS provider.
#[derive(Clone, Debug)]
pub struct VaultConfig {
    /// Vault API endpoint, e.g. `"https://vault.prod:8200"`.
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
/// Current build is a compile-time stub; all operations return
/// [`KmsError::Unavailable`] until the HTTP transport is wired in.
#[derive(Debug)]
pub struct VaultProvider {
    config: VaultConfig,
    epoch: AtomicU64,
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
}

impl TenantKmsProvider for VaultProvider {
    fn wrap(&self, _plaintext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        // Would POST to /v1/<mount>/encrypt/<transit_key> with
        // base64-encoded plaintext and AAD context.
        Err(KmsError::Unavailable(
            "vault HTTP client not compiled — enable kms-vault feature".into(),
        ))
    }

    fn unwrap(&self, _ciphertext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        // Would POST to /v1/<mount>/decrypt/<transit_key>.
        Err(KmsError::Unavailable(
            "vault HTTP client not compiled — enable kms-vault feature".into(),
        ))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        // Would POST to /v1/<mount>/keys/<transit_key>/rotate.
        Err(KmsError::Unavailable(
            "vault HTTP client not compiled — enable kms-vault feature".into(),
        ))
    }

    fn health_check(&self) -> KmsHealth {
        // Would GET /v1/sys/health.
        let _ = self.epoch.load(Ordering::Relaxed);
        KmsHealth::Unavailable("vault provider is compile-time stub".into())
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
            endpoint: "https://vault.test:8200".into(),
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
        assert_eq!(provider.config().endpoint, "https://vault.test:8200");
        assert_eq!(provider.config().transit_key, "kiseki-tenant-key");
        assert_eq!(provider.config().namespace.as_deref(), Some("admin"));
        assert_eq!(provider.name(), "vault-transit");
    }

    #[test]
    fn stub_returns_unavailable() {
        let provider = VaultProvider::new(test_config());

        let wrap_err = provider.wrap(b"plaintext", b"aad").unwrap_err();
        assert!(matches!(wrap_err, KmsError::Unavailable(_)));

        let unwrap_err = provider.unwrap(b"ciphertext", b"aad").unwrap_err();
        assert!(matches!(unwrap_err, KmsError::Unavailable(_)));

        let rotate_err = provider.rotate().unwrap_err();
        assert!(matches!(rotate_err, KmsError::Unavailable(_)));

        assert!(matches!(provider.health_check(), KmsHealth::Unavailable(_)));
    }
}
