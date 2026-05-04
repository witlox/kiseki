#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Block storage device state (ADR-029).

use kiseki_block::{DeviceBackend, Extent};

pub struct BlockState {
    pub device: Option<Box<dyn DeviceBackend>>,
    pub last_extent: Option<Extent>,
    pub device_path: Option<std::path::PathBuf>,
    pub extents: Vec<Extent>,
    pub temp_dir: Option<tempfile::TempDir>,
    pub scrub_report: Option<String>,
}

impl BlockState {
    pub fn new() -> Self {
        Self {
            device: None,
            last_extent: None,
            device_path: None,
            extents: Vec::new(),
            temp_dir: None,
            scrub_report: None,
        }
    }
}
