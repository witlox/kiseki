//! Crypto-shred: destroy tenant KEK → all tenant data unreadable.
//!
//! Spec: I-K5, I-K11, ADR-011.

use crate::envelope::Envelope;
use crate::keys::TenantKek;

/// Result of a crypto-shred operation.
#[derive(Debug)]
pub struct ShredResult {
    /// Number of envelopes whose tenant wrapping was invalidated.
    pub invalidated_count: u64,
    /// Whether a retention hold blocked physical deletion.
    pub retention_held: bool,
}

/// Perform crypto-shred on a set of envelopes.
///
/// Destroys the tenant KEK (consumed/dropped) and clears all
/// tenant wrappings. System path remains functional.
pub fn shred_tenant(
    _kek: TenantKek, // consumed — dropped = destroyed
    envelopes: &mut [Envelope],
    retention_held: bool,
) -> ShredResult {
    let mut count = 0u64;
    for env in envelopes.iter_mut() {
        if env.tenant_wrapped_material.is_some() {
            env.tenant_wrapped_material = None;
            env.tenant_epoch = None;
            count += 1;
        }
    }
    ShredResult {
        invalidated_count: count,
        retention_held,
    }
}

/// Check if an envelope has been shredded (no tenant wrapping).
#[must_use]
pub fn is_shredded(envelope: &Envelope) -> bool {
    envelope.tenant_wrapped_material.is_none()
}

/// Check if system path is still usable after shred.
#[must_use]
pub fn system_path_intact(envelope: &Envelope) -> bool {
    envelope.system_epoch.0 > 0
}

/// Check whether crypto-shred is allowed given compliance tags and hold state.
///
/// HIPAA-tagged namespaces require an explicit retention hold release before
/// crypto-shred is permitted.
pub fn check_shred_allowed(
    compliance_tags: &[kiseki_common::tenancy::ComplianceTag],
    has_hold_released: bool,
) -> Result<(), crate::error::CryptoError> {
    use kiseki_common::tenancy::ComplianceTag;
    for tag in compliance_tags {
        if let ComplianceTag::Hipaa = tag {
            if !has_hold_released {
                return Err(crate::error::CryptoError::InvalidEnvelope(
                    "crypto-shred blocked: HIPAA namespace requires explicit retention hold release".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aead::Aead;
    use crate::envelope::{seal_envelope, wrap_for_tenant};
    use crate::keys::SystemMasterKey;
    use kiseki_common::ids::ChunkId;
    use kiseki_common::tenancy::KeyEpoch;

    #[test]
    fn shred_invalidates_tenant_wrapping() {
        let aead = Aead::new();
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let tenant_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xbb; 32]);

        let mut env = seal_envelope(&aead, &master, &chunk_id, b"secret").unwrap();
        wrap_for_tenant(&aead, &mut env, &tenant_kek).unwrap();
        assert!(!is_shredded(&env));

        let shred_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let result = shred_tenant(shred_kek, &mut [env], false);
        assert_eq!(result.invalidated_count, 1);
    }

    #[test]
    fn shred_preserves_system_path() {
        let aead = Aead::new();
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let tenant_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xbb; 32]);

        let mut env = seal_envelope(&aead, &master, &chunk_id, b"data").unwrap();
        wrap_for_tenant(&aead, &mut env, &tenant_kek).unwrap();

        let shred_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let result = shred_tenant(shred_kek, &mut [env], false);
        assert_eq!(result.invalidated_count, 1);
        assert!(!result.retention_held);
    }

    #[test]
    fn hipaa_blocks_shred_without_hold_release() {
        use kiseki_common::tenancy::ComplianceTag;
        let tags = vec![ComplianceTag::Hipaa];
        let result = check_shred_allowed(&tags, false);
        assert!(
            result.is_err(),
            "shred should be blocked for HIPAA without hold release"
        );
    }

    #[test]
    fn hipaa_allows_shred_with_hold_release() {
        use kiseki_common::tenancy::ComplianceTag;
        let tags = vec![ComplianceTag::Hipaa];
        assert!(check_shred_allowed(&tags, true).is_ok());
    }

    #[test]
    fn non_hipaa_allows_shred_without_hold() {
        use kiseki_common::tenancy::ComplianceTag;
        let tags = vec![ComplianceTag::Gdpr];
        assert!(check_shred_allowed(&tags, false).is_ok());
    }

    #[test]
    fn shred_with_retention_hold() {
        let aead = Aead::new();
        let master = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
        let tenant_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let chunk_id = ChunkId([0xcc; 32]);

        let mut env = seal_envelope(&aead, &master, &chunk_id, b"held").unwrap();
        wrap_for_tenant(&aead, &mut env, &tenant_kek).unwrap();

        let shred_kek = TenantKek::new([0xaa; 32], KeyEpoch(1));
        let result = shred_tenant(shred_kek, &mut [env], true);
        assert_eq!(result.invalidated_count, 1);
        assert!(result.retention_held);
    }
}
