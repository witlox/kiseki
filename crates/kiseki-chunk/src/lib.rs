//! Chunk Storage for Kiseki.
//!
//! Manages encrypted, content-addressed chunks. Chunks are immutable
//! (I-C1), reference-counted (I-C2), placed in affinity pools (I-C3),
//! and protected by retention holds (I-C2b).
//!
//! Invariant mapping:
//!   - I-C1 — chunks immutable; no update API
//!   - I-C2 — no GC while refcount > 0
//!   - I-C2b — no GC while retention hold active
//!   - I-C3 — placement per affinity policy
//!   - I-C4 — EC per pool (durability strategy)

#![deny(unsafe_code)]

pub mod device;
pub mod ec;
pub mod error;
pub mod evacuation;

#[cfg(any(feature = "gpu-cuda", feature = "gpu-rocm", test))]
#[allow(unsafe_code)]
pub mod gpu_direct;
pub mod persistent_store;
pub mod placement;
pub mod pool;
pub mod rebalance;
pub mod scrub_engine;
pub mod small_object_store;
pub mod store;
pub mod striping;

pub use error::ChunkError;
#[cfg(any(feature = "gpu-cuda", feature = "gpu-rocm", test))]
pub use gpu_direct::{GpuBackend, GpuDmaAllocator, GpuDmaBuffer, MockDmaAllocator};
pub use persistent_store::PersistentChunkStore;
pub use pool::{select_pool_for_write, AffinityPool, DeviceClass, DurabilityStrategy};
pub use small_object_store::SmallObjectStore;
pub use store::{ChunkOps, ChunkStore};
