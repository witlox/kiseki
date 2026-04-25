//! Storage administration — pool CRUD, device lifecycle, shard management.
//!
//! Administrative operations for managing the physical storage layer.
//! Restricted to admin/SRE roles (I-Auth3).

use std::collections::HashMap;
use std::sync::RwLock;

use kiseki_common::ids::ShardId;

/// Storage pool definition.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct StoragePool {
    pub name: String,
    pub media_type: MediaType,
    pub device_count: u32,
    pub total_capacity_bytes: u64,
    pub used_bytes: u64,
    pub ec_data_shards: u32,
    pub ec_parity_shards: u32,
}

/// Media type for storage pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    /// `NVMe` SSD.
    Nvme,
    /// SATA/SAS HDD.
    Hdd,
    /// Hybrid (tiered `NVMe` + HDD).
    Hybrid,
}

/// Device status in the admin view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Online and healthy.
    Online,
    /// Draining — no new writes, existing data being migrated.
    Draining,
    /// Offline — not reachable.
    Offline,
    /// Decommissioned — data fully migrated, safe to remove.
    Decommissioned,
}

/// A managed storage device.
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct DeviceInfo {
    pub device_id: String,
    pub pool: String,
    pub status: DeviceStatus,
    pub capacity_bytes: u64,
    pub used_bytes: u64,
}

/// Caller role for authorization checks (I-Auth3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminRole {
    /// Full admin — can manage pools, devices, shards.
    Admin,
    /// SRE — can view and drain, but not delete pools.
    Sre,
    /// Unauthorized caller.
    Unauthorized,
}

/// Storage admin service — manages pools and devices.
pub struct StorageAdminService {
    pools: RwLock<HashMap<String, StoragePool>>,
    devices: RwLock<HashMap<String, DeviceInfo>>,
    shard_assignments: RwLock<HashMap<ShardId, String>>, // shard → pool
    /// Inline data threshold in bytes (ADR-030, I-SF1).
    /// Changes are prospective only — existing deltas are not affected.
    inline_threshold_bytes: RwLock<u64>,
}

/// Check that the caller has admin privileges.
fn require_admin(role: AdminRole) -> Result<(), AdminError> {
    if role == AdminRole::Admin {
        Ok(())
    } else {
        Err(AdminError::Unauthorized)
    }
}

/// Check that the caller has at least SRE privileges.
fn require_sre(role: AdminRole) -> Result<(), AdminError> {
    if role == AdminRole::Admin || role == AdminRole::Sre {
        Ok(())
    } else {
        Err(AdminError::Unauthorized)
    }
}

#[allow(clippy::unwrap_used)]
impl StorageAdminService {
    /// Create an empty admin service.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pools: RwLock::new(HashMap::new()),
            devices: RwLock::new(HashMap::new()),
            shard_assignments: RwLock::new(HashMap::new()),
            inline_threshold_bytes: RwLock::new(4096),
        }
    }

    /// Set the inline data threshold in bytes (ADR-030).
    /// Changes are prospective only — existing deltas are not affected (I-L9).
    pub fn set_inline_threshold(&self, bytes: u64) {
        *self.inline_threshold_bytes.write().unwrap() = bytes;
    }

    /// Get the current inline data threshold in bytes.
    #[must_use]
    pub fn inline_threshold(&self) -> u64 {
        *self.inline_threshold_bytes.read().unwrap()
    }

    /// Attempt to change a tenant quota via `StorageAdminService`.
    /// Always fails — tenant quotas must be changed via `ControlService` (I-Auth3).
    pub fn change_tenant_quota(
        &self,
        _role: AdminRole,
        _tenant_id: &str,
        _new_quota_bytes: u64,
    ) -> Result<(), AdminError> {
        Err(AdminError::NotPermitted(
            "use ControlService for quota changes".into(),
        ))
    }

    /// Create a storage pool. Requires admin role.
    pub fn create_pool(&self, pool: StoragePool, role: AdminRole) -> Result<(), AdminError> {
        require_admin(role)?;
        let mut pools = self.pools.write().unwrap();
        if pools.contains_key(&pool.name) {
            return Err(AdminError::AlreadyExists(pool.name));
        }
        pools.insert(pool.name.clone(), pool);
        Ok(())
    }

    /// Get pool info.
    #[must_use]
    pub fn get_pool(&self, name: &str) -> Option<StoragePool> {
        self.pools.read().unwrap().get(name).cloned()
    }

    /// List all pools.
    #[must_use]
    pub fn list_pools(&self) -> Vec<StoragePool> {
        self.pools.read().unwrap().values().cloned().collect()
    }

    /// Delete a pool (must be empty). Requires admin role.
    pub fn delete_pool(&self, name: &str, role: AdminRole) -> Result<(), AdminError> {
        require_admin(role)?;
        // Acquire devices lock first (consistent order: devices before pools).
        let devices = self.devices.read().unwrap();
        let has_devices = devices.values().any(|d| d.pool == name);
        if has_devices {
            return Err(AdminError::PoolNotEmpty(name.to_owned()));
        }
        // Hold devices lock while removing pool to prevent concurrent add_device.
        let mut pools = self.pools.write().unwrap();
        pools
            .remove(name)
            .ok_or_else(|| AdminError::NotFound(name.to_owned()))?;
        drop(pools);
        drop(devices);
        Ok(())
    }

    /// Add a device to a pool. Requires SRE or admin role.
    pub fn add_device(&self, device: DeviceInfo, role: AdminRole) -> Result<(), AdminError> {
        require_sre(role)?;
        let pools = self.pools.read().unwrap();
        if !pools.contains_key(&device.pool) {
            return Err(AdminError::NotFound(device.pool.clone()));
        }
        // Hold pools lock while inserting device.
        let mut devices = self.devices.write().unwrap();
        devices.insert(device.device_id.clone(), device);
        drop(devices);
        drop(pools);
        Ok(())
    }

    /// Set device status (e.g., start draining). Requires SRE or admin role.
    pub fn set_device_status(
        &self,
        device_id: &str,
        status: DeviceStatus,
        role: AdminRole,
    ) -> Result<(), AdminError> {
        require_sre(role)?;
        let mut devices = self.devices.write().unwrap();
        let dev = devices
            .get_mut(device_id)
            .ok_or_else(|| AdminError::NotFound(device_id.to_owned()))?;
        // Enforce state machine: Online→Draining→Decommissioned, Online↔Offline.
        let valid = matches!(
            (dev.status, status),
            (
                DeviceStatus::Online,
                DeviceStatus::Draining | DeviceStatus::Offline
            ) | (DeviceStatus::Offline, DeviceStatus::Online)
                | (DeviceStatus::Draining, DeviceStatus::Decommissioned)
        );
        if !valid {
            return Err(AdminError::InvalidTransition {
                from: dev.status,
                to: status,
            });
        }
        dev.status = status;
        Ok(())
    }

    /// List devices in a pool.
    #[must_use]
    pub fn list_devices(&self, pool: &str) -> Vec<DeviceInfo> {
        self.devices
            .read()
            .unwrap()
            .values()
            .filter(|d| d.pool == pool)
            .cloned()
            .collect()
    }

    /// Assign a shard to a pool. Requires admin role. Holds pools lock to prevent TOCTOU.
    pub fn assign_shard(
        &self,
        shard_id: ShardId,
        pool: &str,
        role: AdminRole,
    ) -> Result<(), AdminError> {
        require_admin(role)?;
        let pools = self.pools.read().unwrap();
        if !pools.contains_key(pool) {
            return Err(AdminError::NotFound(pool.to_owned()));
        }
        self.shard_assignments
            .write()
            .unwrap()
            .insert(shard_id, pool.to_owned());
        drop(pools);
        Ok(())
    }

    /// Get pool assignment for a shard.
    #[must_use]
    pub fn shard_pool(&self, shard_id: ShardId) -> Option<String> {
        self.shard_assignments
            .read()
            .unwrap()
            .get(&shard_id)
            .cloned()
    }
}

impl Default for StorageAdminService {
    fn default() -> Self {
        Self::new()
    }
}

/// Admin service errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AdminError {
    /// Resource already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// Resource not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Pool still has devices.
    #[error("pool not empty: {0}")]
    PoolNotEmpty(String),
    /// Caller lacks required role.
    #[error("unauthorized")]
    Unauthorized,
    /// Invalid device status transition.
    #[error("invalid transition: {from:?} -> {to:?}")]
    InvalidTransition {
        /// Current status.
        from: DeviceStatus,
        /// Requested status.
        to: DeviceStatus,
    },
    /// Operation not permitted on this service.
    #[error("not permitted: {0}")]
    NotPermitted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> StoragePool {
        StoragePool {
            name: "nvme-fast".into(),
            media_type: MediaType::Nvme,
            device_count: 4,
            total_capacity_bytes: 4_000_000_000_000,
            used_bytes: 0,
            ec_data_shards: 4,
            ec_parity_shards: 2,
        }
    }

    fn test_device(id: &str, pool: &str) -> DeviceInfo {
        DeviceInfo {
            device_id: id.into(),
            pool: pool.into(),
            status: DeviceStatus::Online,
            capacity_bytes: 1_000_000_000_000,
            used_bytes: 0,
        }
    }

    const ADMIN: AdminRole = AdminRole::Admin;

    #[test]
    fn pool_crud() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();

        let pool = svc.get_pool("nvme-fast").unwrap();
        assert_eq!(pool.media_type, MediaType::Nvme);

        let pools = svc.list_pools();
        assert_eq!(pools.len(), 1);

        svc.delete_pool("nvme-fast", ADMIN).unwrap();
        assert!(svc.get_pool("nvme-fast").is_none());
    }

    #[test]
    fn duplicate_pool_rejected() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();
        assert!(svc.create_pool(test_pool(), ADMIN).is_err());
    }

    #[test]
    fn delete_nonempty_pool_rejected() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();
        svc.add_device(test_device("dev-1", "nvme-fast"), ADMIN)
            .unwrap();

        assert!(svc.delete_pool("nvme-fast", ADMIN).is_err());
    }

    #[test]
    fn device_lifecycle() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();
        svc.add_device(test_device("dev-1", "nvme-fast"), ADMIN)
            .unwrap();

        let devices = svc.list_devices("nvme-fast");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].status, DeviceStatus::Online);

        svc.set_device_status("dev-1", DeviceStatus::Draining, ADMIN)
            .unwrap();
        let devices = svc.list_devices("nvme-fast");
        assert_eq!(devices[0].status, DeviceStatus::Draining);
    }

    #[test]
    fn unauthorized_rejected() {
        let svc = StorageAdminService::new();
        assert!(svc.create_pool(test_pool(), AdminRole::Sre).is_err());
        assert!(svc
            .create_pool(test_pool(), AdminRole::Unauthorized)
            .is_err());
    }

    #[test]
    fn invalid_device_transition_rejected() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();
        svc.add_device(test_device("dev-1", "nvme-fast"), ADMIN)
            .unwrap();
        // Online → Decommissioned is not valid (must drain first).
        assert!(svc
            .set_device_status("dev-1", DeviceStatus::Decommissioned, ADMIN)
            .is_err());
    }

    #[test]
    fn shard_assignment() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();

        let shard = ShardId(uuid::Uuid::from_u128(1));
        svc.assign_shard(shard, "nvme-fast", ADMIN).unwrap();
        assert_eq!(svc.shard_pool(shard).unwrap(), "nvme-fast");
    }

    #[test]
    fn assign_to_nonexistent_pool() {
        let svc = StorageAdminService::new();
        let shard = ShardId(uuid::Uuid::from_u128(1));
        assert!(svc.assign_shard(shard, "missing", ADMIN).is_err());
    }

    #[test]
    fn inline_threshold_store_and_retrieve() {
        let svc = StorageAdminService::new();
        // Default is 4096.
        assert_eq!(svc.inline_threshold(), 4096);
        // Set to 8192 and verify.
        svc.set_inline_threshold(8192);
        assert_eq!(svc.inline_threshold(), 8192);
    }

    #[test]
    fn cluster_admin_cannot_modify_tenant_quota() {
        let svc = StorageAdminService::new();
        let result = svc.change_tenant_quota(AdminRole::Admin, "tenant-1", 1_000_000);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, AdminError::NotPermitted(ref msg) if msg.contains("ControlService")),
            "expected NotPermitted error directing to ControlService, got: {err}"
        );
    }

    #[test]
    fn pool_status_has_only_aggregate_fields() {
        // Structural assertion: StoragePool exposes only aggregate capacity
        // fields (total_capacity_bytes, used_bytes, device_count) with no
        // per-tenant breakdown.
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool(), ADMIN).unwrap();

        let pool = svc.get_pool("nvme-fast").unwrap();
        // Access aggregate fields — these must exist.
        let _total = pool.total_capacity_bytes;
        let _used = pool.used_bytes;
        let _devices = pool.device_count;
        // If StoragePool ever gains per-tenant fields this test's compile
        // ensures we consciously update it. The struct has exactly these
        // public data fields (plus name, media_type, ec_*).
        let StoragePool {
            name: _,
            media_type: _,
            device_count: _,
            total_capacity_bytes: _,
            used_bytes: _,
            ec_data_shards: _,
            ec_parity_shards: _,
        } = pool;
        // Destructure succeeds — no per-tenant attribution fields exist.
    }
}
