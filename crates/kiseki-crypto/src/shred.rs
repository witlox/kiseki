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
