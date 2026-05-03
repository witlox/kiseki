//! redb-backed `ViewStorage` impl (ADR-040).
//!
//! Storage layout (single redb file, two tables):
//!
//!   VIEWS: `view_id.0.as_bytes()` → `[1 byte schema_version][postcard(PersistedView)]`
//!   META:  `&str` → variable bytes (`last_applied_seq` only for views)
//!
//! Locks (mirrors the composition equivalent in
//! `kiseki_composition::persistent::redb`):
//!   - `Mutex<Database>` — held only for the duration of one redb
//!     transaction. Never across an await.
//!
//! No LRU cache here: the working set of views is tiny (typically
//! 1 per-tenant + a few service views), so a full hash-table read
//! per lookup is cheap. The composition equivalent caches because
//! it can hold millions of entries.

use std::path::Path;
use std::sync::Mutex;

use ::redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use kiseki_common::ids::{SequenceNumber, ViewId};

use super::storage::{PersistedView, PersistentStoreError, ViewStorage};

// -- Schema -----------------------------------------------------------------

/// Current on-disk schema version for stored `PersistedView`
/// records. Bumped on incompatible changes per ADR-040 §D8.
pub const VIEW_RECORD_SCHEMA_VERSION: u8 = 1;

const VIEWS: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("views");
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");

mod meta_keys {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const LAST_APPLIED_SEQ: &str = "last_applied_seq";
}

// -- Encoding helpers -------------------------------------------------------

fn encode_view(view: &PersistedView) -> Result<Vec<u8>, PersistentStoreError> {
    let mut out = Vec::with_capacity(128);
    out.push(VIEW_RECORD_SCHEMA_VERSION);
    out.extend_from_slice(&postcard::to_stdvec(view)?);
    Ok(out)
}

fn decode_view(bytes: &[u8]) -> Result<PersistedView, PersistentStoreError> {
    let Some((&version, payload)) = bytes.split_first() else {
        return Err(PersistentStoreError::Decode("empty record".to_owned()));
    };
    if version > VIEW_RECORD_SCHEMA_VERSION {
        return Err(PersistentStoreError::SchemaTooNew {
            found: version,
            supported: VIEW_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(postcard::from_bytes(payload)?)
}

fn map_redb<E: std::fmt::Display>(e: E) -> PersistentStoreError {
    PersistentStoreError::Redb(e.to_string())
}

// -- Backend -----------------------------------------------------------------

/// redb-backed `ViewStorage`.
pub struct PersistentRedbStorage {
    db: Mutex<Database>,
}

impl PersistentRedbStorage {
    /// Open or create the views database at `path`. On first open the
    /// schema-version meta key is written; on subsequent opens it's
    /// validated.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, PersistentStoreError> {
        let db = Database::create(path).map_err(map_redb)?;
        // Ensure both tables exist + write the schema version on first
        // open. A single write transaction does both — opening the
        // tables in a write txn creates them if absent.
        {
            let txn = db.begin_write().map_err(map_redb)?;
            {
                let _ = txn.open_table(VIEWS).map_err(map_redb)?;
                let mut meta = txn.open_table(META).map_err(map_redb)?;
                let existing_version = meta
                    .get(meta_keys::SCHEMA_VERSION)
                    .map_err(map_redb)?
                    .and_then(|g| g.value().first().copied());
                if let Some(found) = existing_version {
                    if found > VIEW_RECORD_SCHEMA_VERSION {
                        return Err(PersistentStoreError::SchemaTooNew {
                            found,
                            supported: VIEW_RECORD_SCHEMA_VERSION,
                        });
                    }
                } else {
                    meta.insert(meta_keys::SCHEMA_VERSION, &[VIEW_RECORD_SCHEMA_VERSION][..])
                        .map_err(map_redb)?;
                }
            }
            txn.commit().map_err(map_redb)?;
        }
        Ok(Self { db: Mutex::new(db) })
    }
}

impl ViewStorage for PersistentRedbStorage {
    fn get(&self, id: ViewId) -> Result<Option<PersistedView>, PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_read().map_err(map_redb)?;
        let table = txn.open_table(VIEWS).map_err(map_redb)?;
        let bytes = table
            .get(id.0.as_bytes().as_slice())
            .map_err(map_redb)?
            .map(|g| g.value().to_vec());
        match bytes {
            None => Ok(None),
            Some(b) => Ok(Some(decode_view(&b)?)),
        }
    }

    fn count(&self) -> Result<u64, PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_read().map_err(map_redb)?;
        let table = txn.open_table(VIEWS).map_err(map_redb)?;
        table.len().map_err(map_redb)
    }

    fn list_all(&self) -> Result<Vec<PersistedView>, PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_read().map_err(map_redb)?;
        let table = txn.open_table(VIEWS).map_err(map_redb)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(map_redb)? {
            let (_, val) = entry.map_err(map_redb)?;
            out.push(decode_view(val.value())?);
        }
        Ok(out)
    }

    fn put(&mut self, view: PersistedView) -> Result<(), PersistentStoreError> {
        let bytes = encode_view(&view)?;
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_write().map_err(map_redb)?;
        {
            let mut table = txn.open_table(VIEWS).map_err(map_redb)?;
            table
                .insert(
                    view.descriptor.view_id.0.as_bytes().as_slice(),
                    bytes.as_slice(),
                )
                .map_err(map_redb)?;
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }

    fn remove(&mut self, id: ViewId) -> Result<bool, PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_write().map_err(map_redb)?;
        let removed;
        {
            let mut table = txn.open_table(VIEWS).map_err(map_redb)?;
            removed = table
                .remove(id.0.as_bytes().as_slice())
                .map_err(map_redb)?
                .is_some();
        }
        txn.commit().map_err(map_redb)?;
        Ok(removed)
    }

    fn last_applied_seq(&self) -> Result<SequenceNumber, PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_read().map_err(map_redb)?;
        let table = txn.open_table(META).map_err(map_redb)?;
        let bytes = table
            .get(meta_keys::LAST_APPLIED_SEQ)
            .map_err(map_redb)?
            .map(|g| g.value().to_vec());
        match bytes {
            None => Ok(SequenceNumber(0)),
            Some(b) => {
                if b.len() != 8 {
                    return Err(PersistentStoreError::Decode(format!(
                        "last_applied_seq has {} bytes; expected 8",
                        b.len()
                    )));
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&b);
                Ok(SequenceNumber(u64::from_le_bytes(arr)))
            }
        }
    }

    fn set_last_applied_seq(&mut self, seq: SequenceNumber) -> Result<(), PersistentStoreError> {
        let db = self.db.lock().map_err(|e| map_redb(format!("lock: {e}")))?;
        let txn = db.begin_write().map_err(map_redb)?;
        {
            let mut meta = txn.open_table(META).map_err(map_redb)?;
            meta.insert(meta_keys::LAST_APPLIED_SEQ, &seq.0.to_le_bytes()[..])
                .map_err(map_redb)?;
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }
}
