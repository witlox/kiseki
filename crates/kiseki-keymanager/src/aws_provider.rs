//! AWS KMS provider (feature: kms-aws).
//!
//! Compile-time stub that defines the types and API surface without
//! depending on `aws-sdk-kms`. All operations return
//! [`KmsError::Unavailable`] until the SDK dependency is wired in.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::provider::{KmsEpochId, KmsError, KmsHealth, TenantKmsProvider};

/// Configuration for AWS KMS provider.
#[derive(Clone, Debug)]
pub struct AwsKmsConfig {
    /// KMS key ARN or alias, e.g. `"arn:aws:kms:eu-central-1:123:key/abc"`.
    pub key_id: String,
    /// AWS region, e.g. `"eu-central-1"`.
    pub region: String,
    /// Explicit access key ID (optional; falls back to env/profile).
    pub access_key_id: Option<String>,
    /// Explicit secret access key (optional).
    pub secret_access_key: Option<String>,
    /// Session token for temporary credentials (optional).
    pub session_token: Option<String>,
}

/// AWS KMS provider.
///
/// Current build is a compile-time stub; all operations return
/// [`KmsError::Unavailable`] until `aws-sdk-kms` is added.
#[derive(Debug)]
pub struct AwsKmsProvider {
    config: AwsKmsConfig,
    epoch: AtomicU64,
}

impl AwsKmsProvider {
    /// Create a new AWS KMS provider from the given configuration.
    #[must_use]
    pub fn new(config: AwsKmsConfig) -> Self {
        Self {
            config,
            epoch: AtomicU64::new(1),
        }
    }

    /// Return a reference to the provider configuration.
    #[must_use]
    pub fn config(&self) -> &AwsKmsConfig {
        &self.config
    }
}

impl TenantKmsProvider for AwsKmsProvider {
    fn wrap(&self, _plaintext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        Err(KmsError::Unavailable(
            "aws-sdk-kms not compiled — enable kms-aws feature".into(),
        ))
    }

    fn unwrap(&self, _ciphertext: &[u8], _aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        Err(KmsError::Unavailable(
            "aws-sdk-kms not compiled — enable kms-aws feature".into(),
        ))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        Err(KmsError::Unavailable(
            "aws-sdk-kms not compiled — enable kms-aws feature".into(),
        ))
    }

    fn health_check(&self) -> KmsHealth {
        let _ = self.epoch.load(Ordering::Relaxed);
        KmsHealth::Unavailable("aws-sdk-kms not compiled — enable kms-aws feature".into())
    }

    fn name(&self) -> &'static str {
        "aws-kms"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AwsKmsConfig {
        AwsKmsConfig {
            key_id: "arn:aws:kms:eu-central-1:123456789012:key/test-key-id".into(),
            region: "eu-central-1".into(),
            access_key_id: Some("AKIAIOSFODNN7EXAMPLE".into()),
            secret_access_key: Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()),
            session_token: None,
        }
    }

    #[test]
    fn config_construction() {
        let cfg = test_config();
        let provider = AwsKmsProvider::new(cfg);
        assert_eq!(provider.config().region, "eu-central-1");
        assert!(provider.config().key_id.contains("test-key-id"));
        assert_eq!(provider.name(), "aws-kms");
    }

    #[test]
    fn stub_returns_unavailable() {
        let provider = AwsKmsProvider::new(test_config());

        let wrap_err = provider.wrap(b"plaintext", b"aad").unwrap_err();
        assert!(matches!(wrap_err, KmsError::Unavailable(_)));

        let unwrap_err = provider.unwrap(b"ciphertext", b"aad").unwrap_err();
        assert!(matches!(unwrap_err, KmsError::Unavailable(_)));

        let rotate_err = provider.rotate().unwrap_err();
        assert!(matches!(rotate_err, KmsError::Unavailable(_)));

        assert!(matches!(provider.health_check(), KmsHealth::Unavailable(_)));
    }
}
