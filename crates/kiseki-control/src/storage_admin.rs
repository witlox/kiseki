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

/// Storage admin service — manages pools and devices.
pub struct StorageAdminService {
    pools: RwLock<HashMap<String, StoragePool>>,
    devices: RwLock<HashMap<String, DeviceInfo>>,
    shard_assignments: RwLock<HashMap<ShardId, String>>, // shard → pool
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
        }
    }

    /// Create a storage pool.
    pub fn create_pool(&self, pool: StoragePool) -> Result<(), AdminError> {
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

    /// Delete a pool (must be empty).
    pub fn delete_pool(&self, name: &str) -> Result<(), AdminError> {
        let devices = self.devices.read().unwrap();
        let has_devices = devices.values().any(|d| d.pool == name);
        if has_devices {
            return Err(AdminError::PoolNotEmpty(name.to_owned()));
        }
        drop(devices);

        let mut pools = self.pools.write().unwrap();
        pools
            .remove(name)
            .ok_or_else(|| AdminError::NotFound(name.to_owned()))?;
        Ok(())
    }

    /// Add a device to a pool.
    pub fn add_device(&self, device: DeviceInfo) -> Result<(), AdminError> {
        if !self.pools.read().unwrap().contains_key(&device.pool) {
            return Err(AdminError::NotFound(device.pool.clone()));
        }
        let mut devices = self.devices.write().unwrap();
        devices.insert(device.device_id.clone(), device);
        Ok(())
    }

    /// Set device status (e.g., start draining).
    pub fn set_device_status(
        &self,
        device_id: &str,
        status: DeviceStatus,
    ) -> Result<(), AdminError> {
        let mut devices = self.devices.write().unwrap();
        let dev = devices
            .get_mut(device_id)
            .ok_or_else(|| AdminError::NotFound(device_id.to_owned()))?;
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

    /// Assign a shard to a pool.
    pub fn assign_shard(&self, shard_id: ShardId, pool: &str) -> Result<(), AdminError> {
        if !self.pools.read().unwrap().contains_key(pool) {
            return Err(AdminError::NotFound(pool.to_owned()));
        }
        self.shard_assignments
            .write()
            .unwrap()
            .insert(shard_id, pool.to_owned());
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

    #[test]
    fn pool_crud() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool()).unwrap();

        let pool = svc.get_pool("nvme-fast").unwrap();
        assert_eq!(pool.media_type, MediaType::Nvme);

        let pools = svc.list_pools();
        assert_eq!(pools.len(), 1);

        svc.delete_pool("nvme-fast").unwrap();
        assert!(svc.get_pool("nvme-fast").is_none());
    }

    #[test]
    fn duplicate_pool_rejected() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool()).unwrap();
        assert!(svc.create_pool(test_pool()).is_err());
    }

    #[test]
    fn delete_nonempty_pool_rejected() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool()).unwrap();
        svc.add_device(test_device("dev-1", "nvme-fast")).unwrap();

        assert!(svc.delete_pool("nvme-fast").is_err());
    }

    #[test]
    fn device_lifecycle() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool()).unwrap();
        svc.add_device(test_device("dev-1", "nvme-fast")).unwrap();

        let devices = svc.list_devices("nvme-fast");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].status, DeviceStatus::Online);

        svc.set_device_status("dev-1", DeviceStatus::Draining)
            .unwrap();
        let devices = svc.list_devices("nvme-fast");
        assert_eq!(devices[0].status, DeviceStatus::Draining);
    }

    #[test]
    fn shard_assignment() {
        let svc = StorageAdminService::new();
        svc.create_pool(test_pool()).unwrap();

        let shard = ShardId(uuid::Uuid::from_u128(1));
        svc.assign_shard(shard, "nvme-fast").unwrap();
        assert_eq!(svc.shard_pool(shard).unwrap(), "nvme-fast");
    }

    #[test]
    fn assign_to_nonexistent_pool() {
        let svc = StorageAdminService::new();
        let shard = ShardId(uuid::Uuid::from_u128(1));
        assert!(svc.assign_shard(shard, "missing").is_err());
    }
}
