#![allow(clippy::unwrap_used, clippy::expect_used)]
//! External KMS state (ADR-028).

use std::collections::HashMap;
use std::sync::Arc;

pub struct KmsState {
    pub provider_type: Option<String>,
    pub circuit_open: bool,
    pub concurrent_count: u32,
    pub providers: HashMap<String, Arc<dyn kiseki_keymanager::TenantKmsProvider>>,
}

impl KmsState {
    pub fn new() -> Self {
        let mut providers: HashMap<String, Arc<dyn kiseki_keymanager::TenantKmsProvider>> =
            HashMap::new();
        for name in ["internal", "vault", "kmip", "aws-kms", "pkcs11"] {
            let mut key = vec![0u8; 32];
            for (i, b) in name.bytes().enumerate() {
                key[i % 32] ^= b.wrapping_mul(7);
            }
            providers.insert(
                name.to_string(),
                Arc::new(kiseki_keymanager::InternalProvider::new(key))
                    as Arc<dyn kiseki_keymanager::TenantKmsProvider>,
            );
        }
        Self {
            provider_type: None,
            circuit_open: false,
            concurrent_count: 0,
            providers,
        }
    }
}
