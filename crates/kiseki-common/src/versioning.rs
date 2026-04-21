//! Protocol and format versioning.
//!
//! Tracks wire format versions for backward/forward compatibility.
//! Each serialized structure includes a format_version field so
//! readers can negotiate or reject incompatible formats.

/// Current format version for delta headers.
pub const DELTA_HEADER_FORMAT_VERSION: u32 = 1;

/// Current format version for envelope metadata.
pub const ENVELOPE_FORMAT_VERSION: u32 = 1;

/// Current format version for shard configuration.
pub const SHARD_CONFIG_FORMAT_VERSION: u32 = 1;

/// Version negotiation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionCheck {
    /// Exact match — fully compatible.
    Compatible,
    /// Reader is newer than writer — can read with degraded features.
    ForwardCompatible {
        /// Reader's version.
        reader: u32,
        /// Writer's version.
        writer: u32,
    },
    /// Reader is older than writer — cannot safely read.
    Incompatible {
        /// Reader's version.
        reader: u32,
        /// Writer's version.
        writer: u32,
    },
}

/// Check if a reader version can handle data written with a writer version.
///
/// Policy: readers can handle same version or older writers (forward compatible).
/// Readers cannot handle newer writers (incompatible).
#[must_use]
pub fn check_version(reader_version: u32, writer_version: u32) -> VersionCheck {
    if reader_version == writer_version {
        VersionCheck::Compatible
    } else if reader_version > writer_version {
        VersionCheck::ForwardCompatible {
            reader: reader_version,
            writer: writer_version,
        }
    } else {
        VersionCheck::Incompatible {
            reader: reader_version,
            writer: writer_version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_same_version() {
        assert_eq!(check_version(1, 1), VersionCheck::Compatible);
    }

    #[test]
    fn forward_compatible() {
        assert!(matches!(
            check_version(2, 1),
            VersionCheck::ForwardCompatible {
                reader: 2,
                writer: 1
            }
        ));
    }

    #[test]
    fn incompatible_old_reader() {
        assert!(matches!(
            check_version(1, 2),
            VersionCheck::Incompatible {
                reader: 1,
                writer: 2
            }
        ));
    }

    #[test]
    fn current_versions_are_v1() {
        assert_eq!(DELTA_HEADER_FORMAT_VERSION, 1);
        assert_eq!(ENVELOPE_FORMAT_VERSION, 1);
        assert_eq!(SHARD_CONFIG_FORMAT_VERSION, 1);
    }
}
