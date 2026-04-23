//! GPU-direct storage for zero-copy chunk reads to GPU memory.
//!
//! Bypasses CPU bounce buffers for AI/HPC training data loading.
//! Two backends behind feature flags:
//!
//! - `gpu-cuda`: NVIDIA `GPUDirect` Storage via `cuFile` API
//! - `gpu-rocm`: AMD `ROCm` via HIP memory registration
//!
//! The [`GpuDmaBuffer`] trait abstracts both, allowing the chunk read
//! path to DMA encrypted data directly to GPU memory for in-place
//! decryption.
//!
//! # Architecture
//!
//! ```text
//! NVMe ──DMA──► GPU memory (registered)
//!                    │
//!                    ▼
//!               decrypt in-place (GPU or CPU fallback)
//!                    │
//!                    ▼
//!               application tensor
//! ```

use std::io;

/// A GPU memory buffer registered for DMA transfers.
///
/// Implementations manage GPU memory allocation, registration with the
/// OS/driver for direct I/O, and cleanup on drop. The buffer contents
/// are available to GPU kernels after a successful `read_file` or
/// after being filled via the chunk read path.
pub trait GpuDmaBuffer: Send + Sync {
    /// Raw pointer to the GPU memory region.
    ///
    /// # Safety
    ///
    /// The pointer is valid for the lifetime of this buffer. Callers
    /// must not use the pointer after the buffer is dropped.
    fn as_ptr(&self) -> *mut u8;

    /// Size of the buffer in bytes.
    fn len(&self) -> usize;

    /// Whether the buffer is empty (zero-length).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// GPU device ID this buffer is allocated on.
    fn device_id(&self) -> u32;

    /// Copy data from a host slice into this GPU buffer.
    ///
    /// Used as fallback when DMA is not available (e.g., chunk already
    /// in page cache). Implementations use `cuMemcpyHtoD` (NVIDIA) or
    /// `hipMemcpyHtoD` (AMD).
    fn copy_from_host(&self, src: &[u8]) -> io::Result<()>;

    /// Copy data from this GPU buffer to a host slice.
    ///
    /// Used for verification or when CPU-side processing is needed.
    fn copy_to_host(&self, dst: &mut [u8]) -> io::Result<()>;
}

/// Factory for creating GPU DMA buffers.
///
/// One factory per GPU device. Created at server boot when GPU
/// hardware is detected.
pub trait GpuDmaAllocator: Send + Sync {
    /// The concrete buffer type.
    type Buffer: GpuDmaBuffer;

    /// Allocate a DMA-registered buffer on the GPU.
    ///
    /// # Errors
    ///
    /// Returns an error if GPU memory allocation fails or the buffer
    /// cannot be registered for DMA.
    fn allocate(&self, size: usize) -> io::Result<Self::Buffer>;

    /// GPU device ID this allocator manages.
    fn device_id(&self) -> u32;

    /// Human-readable name (e.g., "NVIDIA A100", "AMD MI300X").
    fn device_name(&self) -> &str;

    /// Backend type identifier.
    fn backend(&self) -> GpuBackend;
}

/// GPU backend type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuBackend {
    /// NVIDIA CUDA / `cuFile` (`GPUDirect` Storage).
    Cuda,
    /// AMD `ROCm` / HIP.
    Rocm,
    /// Mock backend for testing.
    Mock,
}

// ---------------------------------------------------------------------------
// NVIDIA cuFile backend
// ---------------------------------------------------------------------------

/// NVIDIA `GPUDirect` Storage backend.
///
/// Uses the `cuFile` API for direct DMA from `NVMe` to GPU memory.
/// Requires CUDA toolkit, `GPUDirect` Storage driver (`nvidia-gds`),
/// and `NVMe` devices with `GPUDirect` support.
#[cfg(feature = "gpu-cuda")]
pub mod cuda {
    #![allow(unsafe_code)]

    use std::io;

    use super::{GpuBackend, GpuDmaAllocator, GpuDmaBuffer};

    // cuFile FFI declarations.
    mod ffi {
        #![allow(dead_code)]

        /// Opaque cuFile driver handle.
        #[repr(C)]
        pub struct CuFileDriverHandle {
            _opaque: [u8; 0],
        }

        /// cuFile handle for a file descriptor.
        #[repr(C)]
        pub struct CuFileHandle {
            _opaque: [u8; 0],
        }

        /// cuFile status codes.
        pub const CU_FILE_SUCCESS: i32 = 0;

        extern "C" {
            // CUDA runtime.
            pub fn cudaSetDevice(device: libc::c_int) -> libc::c_int;
            pub fn cudaMalloc(devptr: *mut *mut libc::c_void, size: libc::size_t) -> libc::c_int;
            pub fn cudaFree(devptr: *mut libc::c_void) -> libc::c_int;
            pub fn cudaMemcpy(
                dst: *mut libc::c_void,
                src: *const libc::c_void,
                count: libc::size_t,
                kind: libc::c_int,
            ) -> libc::c_int;

            // cuFile API.
            pub fn cuFileDriverOpen() -> libc::c_int;
            pub fn cuFileDriverClose() -> libc::c_int;
            pub fn cuFileBufRegister(
                devptr: *mut libc::c_void,
                size: libc::size_t,
                flags: libc::c_int,
            ) -> libc::c_int;
            pub fn cuFileBufDeregister(devptr: *mut libc::c_void) -> libc::c_int;
            pub fn cuFileRead(
                fh: *mut CuFileHandle,
                devptr: *mut libc::c_void,
                size: libc::size_t,
                file_offset: libc::off_t,
                buf_offset: libc::off_t,
            ) -> libc::ssize_t;
        }

        // cudaMemcpyKind constants.
        pub const CUDA_MEMCPY_H2D: libc::c_int = 1;
        pub const CUDA_MEMCPY_D2H: libc::c_int = 2;

        /// Wrapper for `*mut c_void` that is `Send + Sync`.
        pub struct SafeDevPtr(pub *mut libc::c_void);
        // SAFETY: CUDA device pointers are thread-safe when properly serialized.
        unsafe impl Send for SafeDevPtr {}
        // SAFETY: CUDA device pointers are thread-safe when properly serialized.
        unsafe impl Sync for SafeDevPtr {}
    }

    /// NVIDIA GPU DMA buffer backed by `cudaMalloc` + `cuFileBufRegister`.
    pub struct CudaDmaBuffer {
        ptr: ffi::SafeDevPtr,
        size: usize,
        device_id: u32,
    }

    impl GpuDmaBuffer for CudaDmaBuffer {
        fn as_ptr(&self) -> *mut u8 {
            self.ptr.0.cast()
        }

        fn len(&self) -> usize {
            self.size
        }

        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn copy_from_host(&self, src: &[u8]) -> io::Result<()> {
            if src.len() > self.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source exceeds buffer size",
                ));
            }
            // SAFETY: self.ptr is a valid CUDA device pointer allocated via
            // cudaMalloc. src is a valid host pointer. cudaMemcpy handles
            // the H2D transfer.
            let ret = unsafe {
                ffi::cudaMemcpy(
                    self.ptr.0,
                    src.as_ptr().cast(),
                    src.len(),
                    ffi::CUDA_MEMCPY_H2D,
                )
            };
            if ret != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("cudaMemcpy H2D failed: {ret}"),
                ));
            }
            Ok(())
        }

        fn copy_to_host(&self, dst: &mut [u8]) -> io::Result<()> {
            if dst.len() > self.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination exceeds buffer size",
                ));
            }
            // SAFETY: self.ptr is a valid CUDA device pointer. dst is a valid
            // host buffer. cudaMemcpy handles D2H transfer.
            let ret = unsafe {
                ffi::cudaMemcpy(
                    dst.as_mut_ptr().cast(),
                    self.ptr.0,
                    dst.len(),
                    ffi::CUDA_MEMCPY_D2H,
                )
            };
            if ret != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("cudaMemcpy D2H failed: {ret}"),
                ));
            }
            Ok(())
        }
    }

    impl Drop for CudaDmaBuffer {
        fn drop(&mut self) {
            // SAFETY: self.ptr is a valid registered CUDA device pointer.
            unsafe {
                ffi::cuFileBufDeregister(self.ptr.0);
                ffi::cudaFree(self.ptr.0);
            }
        }
    }

    /// NVIDIA `cuFile` allocator — creates DMA-registered GPU buffers.
    pub struct CudaDmaAllocator {
        device_id: u32,
        device_name: String,
        driver_open: bool,
    }

    impl CudaDmaAllocator {
        /// Initialize the `cuFile` driver and create an allocator for the
        /// given GPU device.
        pub fn new(device_id: u32) -> io::Result<Self> {
            // SAFETY: cudaSetDevice is safe to call with any device ID;
            // it returns an error for invalid IDs.
            let ret = unsafe { ffi::cudaSetDevice(device_id as libc::c_int) };
            if ret != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("cudaSetDevice({device_id}) failed: {ret}"),
                ));
            }

            // SAFETY: cuFileDriverOpen initializes the GDS driver.
            // Safe to call multiple times (idempotent).
            let ret = unsafe { ffi::cuFileDriverOpen() };
            if ret != ffi::CU_FILE_SUCCESS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("cuFileDriverOpen failed: {ret}"),
                ));
            }

            Ok(Self {
                device_id,
                device_name: format!("NVIDIA GPU {device_id}"),
                driver_open: true,
            })
        }
    }

    impl GpuDmaAllocator for CudaDmaAllocator {
        type Buffer = CudaDmaBuffer;

        fn allocate(&self, size: usize) -> io::Result<CudaDmaBuffer> {
            let mut devptr: *mut libc::c_void = std::ptr::null_mut();

            // SAFETY: cudaMalloc allocates device memory and writes
            // the pointer to devptr. We check the return code.
            let ret = unsafe { ffi::cudaMalloc(&mut devptr, size) };
            if ret != 0 || devptr.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!("cudaMalloc({size}) failed: {ret}"),
                ));
            }

            // Register for GDS DMA.
            // SAFETY: devptr is a valid CUDA device pointer just allocated.
            let ret = unsafe { ffi::cuFileBufRegister(devptr, size, 0) };
            if ret != ffi::CU_FILE_SUCCESS {
                // Clean up on failure.
                unsafe {
                    ffi::cudaFree(devptr);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("cuFileBufRegister failed: {ret}"),
                ));
            }

            Ok(CudaDmaBuffer {
                ptr: ffi::SafeDevPtr(devptr),
                size,
                device_id: self.device_id,
            })
        }

        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn device_name(&self) -> &str {
            &self.device_name
        }

        fn backend(&self) -> GpuBackend {
            GpuBackend::Cuda
        }
    }

    impl Drop for CudaDmaAllocator {
        fn drop(&mut self) {
            if self.driver_open {
                // SAFETY: cuFileDriverClose is safe if driver was opened.
                unsafe {
                    ffi::cuFileDriverClose();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AMD ROCm / HIP backend
// ---------------------------------------------------------------------------

/// AMD ROCm backend.
///
/// Uses HIP API for GPU memory allocation and registration.
/// RDMA-to-GPU DMA uses the `amd_peer_direct` kernel module.
/// Requires ROCm toolkit and AMD GPU with `XNACK` support.
#[cfg(feature = "gpu-rocm")]
pub mod rocm {
    #![allow(unsafe_code)]

    use std::io;

    use super::{GpuBackend, GpuDmaAllocator, GpuDmaBuffer};

    // HIP FFI declarations.
    mod ffi {
        #![allow(dead_code)]

        /// HIP error codes.
        pub const HIP_SUCCESS: i32 = 0;

        /// hipMemcpyKind.
        pub const HIP_MEMCPY_H2D: libc::c_int = 1;
        pub const HIP_MEMCPY_D2H: libc::c_int = 2;

        extern "C" {
            pub fn hipSetDevice(device_id: libc::c_int) -> libc::c_int;
            pub fn hipGetDeviceCount(count: *mut libc::c_int) -> libc::c_int;
            pub fn hipMalloc(devptr: *mut *mut libc::c_void, size: libc::size_t) -> libc::c_int;
            pub fn hipFree(devptr: *mut libc::c_void) -> libc::c_int;
            pub fn hipMemcpy(
                dst: *mut libc::c_void,
                src: *const libc::c_void,
                count: libc::size_t,
                kind: libc::c_int,
            ) -> libc::c_int;
            pub fn hipHostRegister(
                hostptr: *mut libc::c_void,
                size: libc::size_t,
                flags: libc::c_uint,
            ) -> libc::c_int;
            pub fn hipHostUnregister(hostptr: *mut libc::c_void) -> libc::c_int;
        }

        /// hipHostRegisterMapped — makes host memory accessible from GPU.
        pub const HIP_HOST_REGISTER_MAPPED: libc::c_uint = 0x02;
        /// hipHostRegisterPortable — valid across all HIP contexts.
        pub const HIP_HOST_REGISTER_PORTABLE: libc::c_uint = 0x01;

        /// Wrapper for `*mut c_void` that is `Send + Sync`.
        pub struct SafeDevPtr(pub *mut libc::c_void);
        // SAFETY: HIP device pointers are thread-safe when properly serialized.
        unsafe impl Send for SafeDevPtr {}
        // SAFETY: HIP device pointers are thread-safe when properly serialized.
        unsafe impl Sync for SafeDevPtr {}
    }

    /// AMD GPU DMA buffer backed by `hipMalloc`.
    pub struct RocmDmaBuffer {
        ptr: ffi::SafeDevPtr,
        size: usize,
        device_id: u32,
    }

    impl GpuDmaBuffer for RocmDmaBuffer {
        fn as_ptr(&self) -> *mut u8 {
            self.ptr.0.cast()
        }

        fn len(&self) -> usize {
            self.size
        }

        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn copy_from_host(&self, src: &[u8]) -> io::Result<()> {
            if src.len() > self.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source exceeds buffer size",
                ));
            }
            // SAFETY: self.ptr is a valid HIP device pointer. src is a valid
            // host buffer. hipMemcpy handles H2D transfer.
            let ret = unsafe {
                ffi::hipMemcpy(
                    self.ptr.0,
                    src.as_ptr().cast(),
                    src.len(),
                    ffi::HIP_MEMCPY_H2D,
                )
            };
            if ret != ffi::HIP_SUCCESS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("hipMemcpy H2D failed: {ret}"),
                ));
            }
            Ok(())
        }

        fn copy_to_host(&self, dst: &mut [u8]) -> io::Result<()> {
            if dst.len() > self.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "destination exceeds buffer size",
                ));
            }
            // SAFETY: self.ptr is a valid HIP device pointer. dst is a valid
            // host buffer. hipMemcpy handles D2H transfer.
            let ret = unsafe {
                ffi::hipMemcpy(
                    dst.as_mut_ptr().cast(),
                    self.ptr.0,
                    dst.len(),
                    ffi::HIP_MEMCPY_D2H,
                )
            };
            if ret != ffi::HIP_SUCCESS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("hipMemcpy D2H failed: {ret}"),
                ));
            }
            Ok(())
        }
    }

    impl Drop for RocmDmaBuffer {
        fn drop(&mut self) {
            // SAFETY: self.ptr is a valid HIP device pointer.
            unsafe {
                ffi::hipFree(self.ptr.0);
            }
        }
    }

    /// AMD ROCm allocator — creates DMA-capable GPU buffers.
    pub struct RocmDmaAllocator {
        device_id: u32,
        device_name: String,
    }

    impl RocmDmaAllocator {
        /// Initialize HIP and create an allocator for the given GPU device.
        pub fn new(device_id: u32) -> io::Result<Self> {
            // Verify device exists.
            let mut count: libc::c_int = 0;
            // SAFETY: hipGetDeviceCount writes to a valid pointer.
            let ret = unsafe { ffi::hipGetDeviceCount(&mut count) };
            if ret != ffi::HIP_SUCCESS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("hipGetDeviceCount failed: {ret}"),
                ));
            }
            if device_id >= count as u32 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("GPU {device_id} not found (have {count})"),
                ));
            }

            // SAFETY: hipSetDevice is safe for any valid device ID.
            let ret = unsafe { ffi::hipSetDevice(device_id as libc::c_int) };
            if ret != ffi::HIP_SUCCESS {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("hipSetDevice({device_id}) failed: {ret}"),
                ));
            }

            Ok(Self {
                device_id,
                device_name: format!("AMD GPU {device_id}"),
            })
        }
    }

    impl GpuDmaAllocator for RocmDmaAllocator {
        type Buffer = RocmDmaBuffer;

        fn allocate(&self, size: usize) -> io::Result<RocmDmaBuffer> {
            let mut devptr: *mut libc::c_void = std::ptr::null_mut();

            // SAFETY: hipMalloc allocates device memory and writes
            // the pointer to devptr. We check the return code.
            let ret = unsafe { ffi::hipMalloc(&mut devptr, size) };
            if ret != ffi::HIP_SUCCESS || devptr.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!("hipMalloc({size}) failed: {ret}"),
                ));
            }

            Ok(RocmDmaBuffer {
                ptr: ffi::SafeDevPtr(devptr),
                size,
                device_id: self.device_id,
            })
        }

        fn device_id(&self) -> u32 {
            self.device_id
        }

        fn device_name(&self) -> &str {
            &self.device_name
        }

        fn backend(&self) -> GpuBackend {
            GpuBackend::Rocm
        }
    }
}

// ---------------------------------------------------------------------------
// Mock backend (for testing without GPU hardware)
// ---------------------------------------------------------------------------

/// Mock GPU DMA buffer backed by host memory.
///
/// Used in unit tests and CI where no GPU hardware is available.
pub struct MockDmaBuffer {
    data: Vec<u8>,
    device_id: u32,
}

impl GpuDmaBuffer for MockDmaBuffer {
    fn as_ptr(&self) -> *mut u8 {
        self.data.as_ptr().cast_mut()
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn device_id(&self) -> u32 {
        self.device_id
    }

    fn copy_from_host(&self, src: &[u8]) -> io::Result<()> {
        if src.len() > self.data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source exceeds buffer size",
            ));
        }
        // SAFETY: mock buffer — just memcpy. The trait says &self but we
        // need interior mutability for the mock. Use raw pointer.
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.data.as_ptr().cast_mut(), src.len());
        }
        Ok(())
    }

    fn copy_to_host(&self, dst: &mut [u8]) -> io::Result<()> {
        let copy_len = dst.len().min(self.data.len());
        dst[..copy_len].copy_from_slice(&self.data[..copy_len]);
        Ok(())
    }
}

/// Mock allocator for testing.
pub struct MockDmaAllocator {
    device_id: u32,
}

impl MockDmaAllocator {
    /// Create a mock allocator.
    #[must_use]
    pub fn new(device_id: u32) -> Self {
        Self { device_id }
    }
}

impl GpuDmaAllocator for MockDmaAllocator {
    type Buffer = MockDmaBuffer;

    fn allocate(&self, size: usize) -> io::Result<MockDmaBuffer> {
        Ok(MockDmaBuffer {
            data: vec![0u8; size],
            device_id: self.device_id,
        })
    }

    fn device_id(&self) -> u32 {
        self.device_id
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn device_name(&self) -> &str {
        "Mock GPU"
    }

    fn backend(&self) -> GpuBackend {
        GpuBackend::Mock
    }
}

// ---------------------------------------------------------------------------
// GPU device detection
// ---------------------------------------------------------------------------

/// Detected GPU device info.
#[derive(Clone, Debug)]
pub struct GpuDeviceInfo {
    /// Device index.
    pub device_id: u32,
    /// Backend type.
    pub backend: GpuBackend,
    /// Human-readable name.
    pub name: String,
}

/// Detect available GPU devices on this system.
///
/// Checks for NVIDIA GPUs via `nvidia-smi` and AMD GPUs via KFD sysfs.
#[must_use]
pub fn detect_gpu_devices() -> Vec<GpuDeviceInfo> {
    let mut devices = Vec::new();

    // NVIDIA: check /proc/driver/nvidia/gpus/ or nvidia-smi.
    let nvidia_dir = std::path::Path::new("/proc/driver/nvidia/gpus");
    if nvidia_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(nvidia_dir) {
            for (i, entry) in entries.flatten().enumerate() {
                devices.push(GpuDeviceInfo {
                    #[allow(clippy::cast_possible_truncation)]
                    device_id: i as u32,
                    backend: GpuBackend::Cuda,
                    name: entry.file_name().to_string_lossy().into_owned(),
                });
            }
        }
    }

    // AMD: check /sys/class/kfd/kfd/topology/nodes/*/properties.
    let kfd_dir = std::path::Path::new("/sys/class/kfd/kfd/topology/nodes");
    if kfd_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(kfd_dir) {
            for entry in entries.flatten() {
                let props_path = entry.path().join("properties");
                if let Ok(content) = std::fs::read_to_string(&props_path) {
                    // Only count nodes with simd_count > 0 (actual GPUs, not CPUs).
                    let is_gpu = content
                        .lines()
                        .any(|l| l.starts_with("simd_count") && !l.ends_with(" 0"));
                    if is_gpu {
                        let id = entry
                            .file_name()
                            .to_string_lossy()
                            .parse::<u32>()
                            .unwrap_or(0);
                        devices.push(GpuDeviceInfo {
                            device_id: id,
                            backend: GpuBackend::Rocm,
                            name: format!("AMD GPU node {id}"),
                        });
                    }
                }
            }
        }
    }

    devices
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_allocate_and_copy() {
        let alloc = MockDmaAllocator::new(0);
        let buf = alloc.allocate(1024).unwrap();
        assert_eq!(buf.len(), 1024);
        assert!(!buf.is_empty());
        assert_eq!(buf.device_id(), 0);

        // Write and read back.
        let data = vec![0xAB_u8; 512];
        buf.copy_from_host(&data).unwrap();

        let mut readback = vec![0u8; 512];
        buf.copy_to_host(&mut readback).unwrap();
        assert_eq!(readback, data);
    }

    #[test]
    fn mock_oversized_copy_fails() {
        let alloc = MockDmaAllocator::new(0);
        let buf = alloc.allocate(64).unwrap();

        let data = vec![0u8; 128]; // larger than buffer
        assert!(buf.copy_from_host(&data).is_err());
    }

    #[test]
    fn mock_empty_buffer() {
        let alloc = MockDmaAllocator::new(0);
        let buf = alloc.allocate(0).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn mock_backend_type() {
        let alloc = MockDmaAllocator::new(0);
        assert_eq!(alloc.backend(), GpuBackend::Mock);
        assert_eq!(alloc.device_name(), "Mock GPU");
    }

    #[test]
    fn detect_gpu_does_not_panic() {
        // On CI without GPU hardware, should return empty vec.
        let devices = detect_gpu_devices();
        let _ = devices;
    }

    #[test]
    fn gpu_backend_equality() {
        assert_eq!(GpuBackend::Cuda, GpuBackend::Cuda);
        assert_ne!(GpuBackend::Cuda, GpuBackend::Rocm);
        assert_ne!(GpuBackend::Rocm, GpuBackend::Mock);
    }
}
