//! On-disk superblock — per-device metadata at offset 0.

use crate::error::BlockError;

/// Superblock magic bytes.
pub const MAGIC: [u8; 8] = *b"KISEKI\x01\x00";

/// Current format version.
pub const FORMAT_VERSION: u32 = 1;

/// Superblock is always one block (minimum 4K).
pub const SUPERBLOCK_SIZE: u64 = 4096;

/// On-disk superblock layout.
#[derive(Clone, Debug)]
pub struct Superblock {
    /// Magic bytes — must be [`MAGIC`].
    pub magic: [u8; 8],
    /// Format version.
    pub version: u32,
    /// Device UUID.
    pub device_id: [u8; 16],
    /// Physical block size in bytes (probed from device).
    pub block_size: u32,
    /// Total number of allocatable blocks in the data region.
    pub total_blocks: u64,
    /// Byte offset of primary allocation bitmap.
    pub bitmap_offset: u64,
    /// Byte offset of mirror allocation bitmap.
    pub bitmap_mirror_offset: u64,
    /// Size of each bitmap in blocks.
    pub bitmap_blocks: u64,
    /// Byte offset of the data region (first allocatable block).
    pub data_offset: u64,
    /// Monotonic generation counter (incremented on bitmap flush).
    pub generation: u64,
}

impl Superblock {
    /// Create a new superblock for a device of `device_bytes` total size.
    #[must_use]
    pub fn new(device_bytes: u64, block_size: u32) -> Self {
        let bs = u64::from(block_size);

        // Bitmap: 1 bit per data block. We need to compute how many
        // data blocks fit after superblock + 2×bitmap.
        // Approximate: bitmap_bits ≈ (device_bytes - superblock) / (block_size + 2/8)
        // Simplified: over-allocate bitmap slightly.
        let usable = device_bytes.saturating_sub(SUPERBLOCK_SIZE);
        let approx_data_blocks = usable / bs;
        let bitmap_bytes = approx_data_blocks.div_ceil(8);
        let bitmap_blocks = bitmap_bytes.div_ceil(bs);

        let bitmap_offset = SUPERBLOCK_SIZE;
        let bitmap_mirror_offset = bitmap_offset + bitmap_blocks * bs;
        let data_offset = bitmap_mirror_offset + bitmap_blocks * bs;

        // Actual data blocks (accounting for superblock + 2×bitmap).
        let total_blocks = (device_bytes.saturating_sub(data_offset)) / bs;

        Self {
            magic: MAGIC,
            version: FORMAT_VERSION,
            device_id: *uuid::Uuid::new_v4().as_bytes(),
            block_size,
            total_blocks,
            bitmap_offset,
            bitmap_mirror_offset,
            bitmap_blocks,
            data_offset,
            generation: 0,
        }
    }

    /// Serialize the superblock to a fixed-size byte buffer.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        #[allow(clippy::cast_possible_truncation)] // SUPERBLOCK_SIZE is 4096, always fits in usize
        let mut buf = vec![0u8; SUPERBLOCK_SIZE as usize];
        buf[0..8].copy_from_slice(&self.magic);
        buf[8..12].copy_from_slice(&self.version.to_le_bytes());
        buf[12..28].copy_from_slice(&self.device_id);
        buf[28..32].copy_from_slice(&self.block_size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.total_blocks.to_le_bytes());
        buf[40..48].copy_from_slice(&self.bitmap_offset.to_le_bytes());
        buf[48..56].copy_from_slice(&self.bitmap_mirror_offset.to_le_bytes());
        buf[56..64].copy_from_slice(&self.bitmap_blocks.to_le_bytes());
        buf[64..72].copy_from_slice(&self.data_offset.to_le_bytes());
        buf[72..80].copy_from_slice(&self.generation.to_le_bytes());
        // Bytes 80..4096 reserved (zero-filled).
        buf
    }

    /// Parse a superblock from bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, BlockError> {
        #[allow(clippy::cast_possible_truncation)] // SUPERBLOCK_SIZE is 4096, always fits in usize
        if buf.len() < SUPERBLOCK_SIZE as usize {
            return Err(BlockError::InvalidSuperblock("buffer too small".into()));
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if magic != MAGIC {
            return Err(BlockError::NotInitialized);
        }

        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(BlockError::InvalidSuperblock(format!(
                "unsupported version {version}"
            )));
        }

        let mut device_id = [0u8; 16];
        device_id.copy_from_slice(&buf[12..28]);

        let block_size = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        let total_blocks = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let bitmap_offset = u64::from_le_bytes(buf[40..48].try_into().unwrap());
        let bitmap_mirror_offset = u64::from_le_bytes(buf[48..56].try_into().unwrap());
        let bitmap_blocks = u64::from_le_bytes(buf[56..64].try_into().unwrap());
        let data_offset = u64::from_le_bytes(buf[64..72].try_into().unwrap());
        let generation = u64::from_le_bytes(buf[72..80].try_into().unwrap());

        // Validate parsed fields.
        if block_size == 0 || !block_size.is_power_of_two() {
            return Err(BlockError::InvalidSuperblock(format!(
                "block_size must be a positive power of two, got {block_size}"
            )));
        }
        if bitmap_offset < SUPERBLOCK_SIZE {
            return Err(BlockError::InvalidSuperblock(format!(
                "bitmap_offset ({bitmap_offset}) must be >= SUPERBLOCK_SIZE ({SUPERBLOCK_SIZE})"
            )));
        }
        if bitmap_mirror_offset <= bitmap_offset {
            return Err(BlockError::InvalidSuperblock(format!(
                "bitmap_mirror_offset ({bitmap_mirror_offset}) must be > bitmap_offset ({bitmap_offset})"
            )));
        }
        if data_offset <= bitmap_mirror_offset {
            return Err(BlockError::InvalidSuperblock(format!(
                "data_offset ({data_offset}) must be > bitmap_mirror_offset ({bitmap_mirror_offset})"
            )));
        }
        if total_blocks == 0 {
            return Err(BlockError::InvalidSuperblock(
                "total_blocks must be > 0".into(),
            ));
        }

        Ok(Self {
            magic,
            version,
            device_id,
            block_size,
            total_blocks,
            bitmap_offset,
            bitmap_mirror_offset,
            bitmap_blocks,
            data_offset,
            generation,
        })
    }

    /// Byte offset for a given data block index.
    #[must_use]
    pub fn block_offset(&self, block_index: u64) -> u64 {
        self.data_offset + block_index * u64::from(self.block_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sb = Superblock::new(1024 * 1024 * 1024, 4096); // 1GB, 4K blocks
        let bytes = sb.to_bytes();
        let parsed = Superblock::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.magic, MAGIC);
        assert_eq!(parsed.version, FORMAT_VERSION);
        assert_eq!(parsed.block_size, 4096);
        assert!(parsed.total_blocks > 0);
        assert!(parsed.data_offset > parsed.bitmap_mirror_offset);
    }

    #[test]
    fn invalid_magic_returns_not_initialized() {
        let buf = vec![0u8; 4096];
        assert!(matches!(
            Superblock::from_bytes(&buf),
            Err(BlockError::NotInitialized)
        ));
    }

    #[test]
    fn data_region_after_two_bitmaps() {
        let sb = Superblock::new(4 * 1024 * 1024 * 1024, 4096); // 4GB
        assert!(sb.bitmap_mirror_offset > sb.bitmap_offset);
        assert!(sb.data_offset > sb.bitmap_mirror_offset);
        assert!(sb.total_blocks > 0);
    }
}
