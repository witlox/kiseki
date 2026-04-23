//! AWS KMS provider (feature: `kms-aws`).
//!
//! Request construction is real (correct JSON bodies, base64 encoding).
//! The actual HTTP call returns [`KmsError::Unavailable`] — full `SigV4`
//! signing requires the AWS SDK or a substantial signing implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use base64::prelude::*;

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
/// Request construction is functional; HTTP transport returns
/// `Unavailable` until aws-sdk-kms or `SigV4` signing is linked.
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

    /// Build the JSON body for an AWS KMS Encrypt request.
    #[must_use]
    pub fn build_encrypt_body(key_id: &str, plaintext: &[u8], aad: &[u8]) -> String {
        let pt_b64 = BASE64_STANDARD.encode(plaintext);
        let aad_b64 = BASE64_STANDARD.encode(aad);
        format!(
            r#"{{"KeyId":"{key_id}","Plaintext":"{pt_b64}","EncryptionContext":{{"aad":"{aad_b64}"}}}}"#
        )
    }

    /// Build the JSON body for an AWS KMS Decrypt request.
    #[must_use]
    pub fn build_decrypt_body(key_id: &str, ciphertext_blob: &[u8], aad: &[u8]) -> String {
        let ct_b64 = BASE64_STANDARD.encode(ciphertext_blob);
        let aad_b64 = BASE64_STANDARD.encode(aad);
        format!(
            r#"{{"KeyId":"{key_id}","CiphertextBlob":"{ct_b64}","EncryptionContext":{{"aad":"{aad_b64}"}}}}"#
        )
    }

    /// The KMS endpoint for the configured region.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("https://kms.{}.amazonaws.com", self.config.region)
    }
}

impl TenantKmsProvider for AwsKmsProvider {
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        // Request construction is real — body is valid AWS KMS JSON.
        let _body = Self::build_encrypt_body(&self.config.key_id, plaintext, aad);
        let _endpoint = self.endpoint();
        Err(KmsError::Unavailable(
            "aws-sdk-kms not linked — SigV4 signing required".into(),
        ))
    }

    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError> {
        let _body = Self::build_decrypt_body(&self.config.key_id, ciphertext, aad);
        let _endpoint = self.endpoint();
        Err(KmsError::Unavailable(
            "aws-sdk-kms not linked — SigV4 signing required".into(),
        ))
    }

    fn rotate(&self) -> Result<KmsEpochId, KmsError> {
        let _ = self.epoch.load(Ordering::Relaxed);
        Err(KmsError::Unavailable(
            "aws-sdk-kms not linked — SigV4 signing required".into(),
        ))
    }

    fn health_check(&self) -> KmsHealth {
        let _ = self.epoch.load(Ordering::Relaxed);
        KmsHealth::Unavailable("aws-sdk-kms not linked — SigV4 signing required".into())
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
        assert_eq!(
            provider.endpoint(),
            "https://kms.eu-central-1.amazonaws.com"
        );
    }

    #[test]
    fn name_is_aws_kms() {
        let provider = AwsKmsProvider::new(test_config());
        assert_eq!(provider.name(), "aws-kms");
    }

    #[test]
    fn encrypt_body_format() {
        let body = AwsKmsProvider::build_encrypt_body("key-1", b"secret", b"aad-data");
        assert!(body.contains("\"KeyId\":\"key-1\""));
        assert!(body.contains("\"Plaintext\":\""));
        assert!(body.contains("\"EncryptionContext\""));

        // Verify base64 is decodable.
        // The plaintext field value is base64("secret") = "c2VjcmV0"
        assert!(body.contains("c2VjcmV0"));
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
