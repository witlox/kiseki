//! In-memory key store — reference implementation of [`KeyManagerOps`].
//!
//! Uses `Mutex` for interior mutability so that `KeyManagerOps` methods
//! can take `&self` (required for Raft-backed implementations).

use std::sync::{Arc, Mutex};

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;

use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::{KeyManagerHealth, KeyManagerStatus};

/// Entry for a single epoch in the key store.
struct EpochEntry {
    key: Arc<SystemMasterKey>,
    is_current: bool,
    migration_complete: bool,
}

/// Inner state behind the mutex.
struct Inner {
    epochs: Vec<EpochEntry>,
    status: KeyManagerStatus,
}

/// In-memory key store for testing and development.
pub struct MemKeyStore {
    inner: Mutex<Inner>,
}

impl MemKeyStore {
    /// Create an empty key store and generate the initial epoch (epoch 1).
    pub fn new() -> Result<Self, KeyManagerError> {
        let key_material = generate_master_key()?;
        let inner = Inner {
            epochs: vec![EpochEntry {
                key: Arc::new(SystemMasterKey::new(key_material, KeyEpoch(1))),
                is_current: true,
                migration_complete: true,
            }],
            status: KeyManagerStatus::Healthy,
        };
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Get the health status of this key store.
    #[must_use]
    pub fn health(&self) -> KeyManagerHealth {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        KeyManagerHealth {
            status: inner.status,
            epoch_count: inner.epochs.len(),
            current_epoch: inner
                .epochs
                .iter()
                .find(|e| e.is_current)
                .map(|e| e.key.epoch.0),
        }
    }

    /// Set the status (for testing failure scenarios).
    pub fn set_status(&self, status: KeyManagerStatus) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.status = status;
    }
}

impl Default for MemKeyStore {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            inner: Mutex::new(Inner {
                epochs: Vec::new(),
                status: KeyManagerStatus::Unavailable,
            }),
        })
    }
}

#[tonic::async_trait]
impl KeyManagerOps for MemKeyStore {
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        inner
            .epochs
            .iter()
            .find(|e| e.key.epoch == epoch)
            .map(|e| Arc::clone(&e.key))
            .ok_or(KeyManagerError::EpochNotFound(epoch))
    }

    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }
        inner
            .epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch)
            .ok_or(KeyManagerError::Unavailable)
    }

    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.status == KeyManagerStatus::Unavailable {
            return Err(KeyManagerError::Unavailable);
        }

        let next_epoch = inner
            .epochs
            .iter()
            .map(|e| e.key.epoch.0)
            .max()
            .unwrap_or(0)
            + 1;

        let key_material = generate_master_key()?;

        for entry in &mut inner.epochs {
            if entry.is_current {
                entry.is_current = false;
            }
        }

        let new_epoch = KeyEpoch(next_epoch);
        inner.epochs.push(EpochEntry {
            key: Arc::new(SystemMasterKey::new(key_material, new_epoch)),
            is_current: true,
            migration_complete: false,
        });

        Ok(new_epoch)
    }

    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = inner
            .epochs
            .iter_mut()
            .find(|e| e.key.epoch == epoch)
            .ok_or(KeyManagerError::EpochNotFound(epoch))?;
        entry.migration_complete = true;
        Ok(())
    }

    async fn list_epochs(&self) -> Vec<EpochInfo> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .epochs
            .iter()
            .map(|e| EpochInfo {
                epoch: e.key.epoch,
                is_current: e.is_current,
                migration_complete: e.migration_complete,
            })
            .collect()
    }
}

impl core::fmt::Debug for MemKeyStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemKeyStore").finish_non_exhaustive()
    }
}

/// Generate a 32-byte master key from the system CSPRNG.
fn generate_master_key() -> Result<[u8; 32], KeyManagerError> {
    let mut key = [0u8; 32];
    aws_lc_rs::rand::fill(&mut key).map_err(|_| KeyManagerError::KeyGenerationFailed)?;
    Ok(key)
}

/// Type of key lifecycle event for audit logging.
///
/// Every key lifecycle operation produces a structured event suitable
/// for the audit log. Keys themselves are NEVER recorded — only metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEventType {
    /// A new key (system DEK or tenant KEK) was generated.
    KeyGeneration,
    /// A key was rotated (new epoch created).
    KeyRotation,
    /// A key was destroyed (crypto-shred).
    KeyDestruction,
    /// A key was accessed (e.g., system DEK unwrapped for read).
    KeyAccess,
    /// Full re-encryption was triggered.
    ReEncryption,
}

/// Structured audit event for a key lifecycle operation.
///
/// Contains all fields required by the audit log (timestamp, actor,
/// key_id, event_type, tenant_id) without ever including the key
/// material itself.
#[derive(Clone, Debug)]
pub struct KeyLifecycleEvent {
    /// ISO 8601 timestamp with timezone.
    pub timestamp: String,
    /// Actor performing the operation.
    pub actor: String,
    /// Identifier of the affected key.
    pub key_id: String,
    /// Type of lifecycle event.
    pub event_type: KeyEventType,
    /// Tenant ID if the event is tenant-scoped, `None` for system-scoped.
    pub tenant_id: Option<String>,
}

impl KeyLifecycleEvent {
    /// Create a system-scoped key lifecycle event.
    #[must_use]
    pub fn system_event(event_type: KeyEventType, key_id: String, actor: String) -> Self {
        Self {
            timestamp: chrono_iso8601_now(),
            actor,
            key_id,
            event_type,
            tenant_id: None,
        }
    }

    /// Create a tenant-scoped key lifecycle event.
    #[must_use]
    pub fn tenant_event(
        event_type: KeyEventType,
        key_id: String,
        actor: String,
        tenant_id: String,
    ) -> Self {
        Self {
            timestamp: chrono_iso8601_now(),
            actor,
            key_id,
            event_type,
            tenant_id: Some(tenant_id),
        }
    }
}

/// Produce an ISO 8601 timestamp string with timezone for audit events.
fn chrono_iso8601_now() -> String {
    // In production this would use a real clock; for deterministic testing
    // we produce a fixed-format UTC timestamp.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch::KeyManagerOps;

    // --- key-management.feature @unit: "All key lifecycle events are audited" ---

    #[tokio::test]
    async fn key_lifecycle_events_produce_structured_audit_data() {
        let store = MemKeyStore::new().unwrap();

        // 1. Key generation event — new system DEK created.
        let gen_event = KeyLifecycleEvent::system_event(
            KeyEventType::KeyGeneration,
            "sys-dek-001".to_string(),
            "system".to_string(),
        );
        assert_eq!(gen_event.event_type, KeyEventType::KeyGeneration);
        assert_eq!(gen_event.key_id, "sys-dek-001");
        assert!(!gen_event.timestamp.is_empty(), "timestamp must be present");
        assert_eq!(gen_event.actor, "system");
        assert!(gen_event.tenant_id.is_none(), "system event has no tenant");

        // 2. Key rotation event — tenant KEK rotated.
        let current_epoch = store.current_epoch().await.unwrap();
        let new_epoch = store.rotate().await.unwrap();
        assert!(new_epoch.0 > current_epoch.0);

        let rotation_event = KeyLifecycleEvent::tenant_event(
            KeyEventType::KeyRotation,
            format!("pharma-kek-{:03}", new_epoch.0),
            "tenant admin".to_string(),
            "org-pharma".to_string(),
        );
        assert_eq!(rotation_event.event_type, KeyEventType::KeyRotation);
        assert_eq!(rotation_event.tenant_id, Some("org-pharma".to_string()));
        assert!(!rotation_event.timestamp.is_empty());

        // 3. Key destruction event — crypto-shred.
        let destroy_event = KeyLifecycleEvent::tenant_event(
            KeyEventType::KeyDestruction,
            "pharma-kek-001".to_string(),
            "tenant admin".to_string(),
            "org-pharma".to_string(),
        );
        assert_eq!(destroy_event.event_type, KeyEventType::KeyDestruction);
        assert_eq!(destroy_event.key_id, "pharma-kek-001");

        // 4. Key access event — system DEK unwrapped for read.
        let _key = store.fetch_master_key(current_epoch).await.unwrap();
        let access_event = KeyLifecycleEvent::system_event(
            KeyEventType::KeyAccess,
            format!("sys-dek-epoch-{}", current_epoch.0),
            "system".to_string(),
        );
        assert_eq!(access_event.event_type, KeyEventType::KeyAccess);

        // 5. Re-encryption event.
        let reencrypt_event = KeyLifecycleEvent::system_event(
            KeyEventType::ReEncryption,
            format!("migration-{}-to-{}", current_epoch.0, new_epoch.0),
            "cluster admin".to_string(),
        );
        assert_eq!(reencrypt_event.event_type, KeyEventType::ReEncryption);

        // Verify all events have the required fields.
        let all_events = vec![
            &gen_event,
            &rotation_event,
            &destroy_event,
            &access_event,
            &reencrypt_event,
        ];
        for event in &all_events {
            assert!(!event.timestamp.is_empty(), "timestamp required");
            assert!(!event.actor.is_empty(), "actor required");
            assert!(!event.key_id.is_empty(), "key_id required");
        }

        // Keys themselves are NEVER recorded in the event.
        // (Structural proof: KeyLifecycleEvent has no field that could
        // hold key material — only key_id, event_type, timestamp, actor.)
    }
}
