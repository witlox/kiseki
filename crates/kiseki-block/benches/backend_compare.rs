//! `FileBackedDevice` vs `UringFileBackedDevice` criterion bench.
//!
//! Promotes / blocks the io_uring backend per GH #39:
//!
//!   - Acceptance gate: ≥ 20 % single-fsync write win at 4 KB on a
//!     real NVMe.
//!   - 64 KB shape: no regression.
//!
//! Runs:
//!   `cargo bench -p kiseki-block --features io_uring`
//!
//! Skips the uring half when the `io_uring` feature is off (default),
//! so `cargo bench -p kiseki-block` is still meaningful on
//! non-Linux / older-kernel hosts. The `FileBackedDevice` half always
//! runs as the baseline.
//!
//! Where the bench is run:
//!   $TMPDIR (or /tmp). The acceptance threshold is only meaningful
//!   when $TMPDIR points at an NVMe — on a tmpfs (RAM) host fsync is
//!   ~free and the win shrinks. The PR records the medium used.

#![allow(clippy::unwrap_used)] // bench code — panic-on-error is fine.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kiseki_block::{DeviceBackend, FileBackedDevice};
use tempfile::tempdir;

#[cfg(feature = "io_uring")]
use kiseki_block::UringFileBackedDevice;

const DEV_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB — enough for many extents.
const SHAPES: &[usize] = &[4 * 1024, 64 * 1024];

fn bench_write_fsync(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_fsync");
    group.sample_size(20);

    for &shape in SHAPES {
        let payload = vec![0xA5u8; shape];

        // Baseline: FileBackedDevice.
        group.throughput(Throughput::Bytes(shape as u64));
        group.bench_with_input(BenchmarkId::new("file", shape), &payload, |b, payload| {
            let dir = tempdir().unwrap();
            let dev = FileBackedDevice::init(&dir.path().join("bench.dev"), DEV_SIZE).unwrap();
            b.iter(|| {
                let ext = dev.alloc(payload.len() as u64).unwrap();
                dev.write(&ext, black_box(payload)).unwrap();
                dev.sync().unwrap();
                // Free so the device doesn't fill up over the
                // measurement window. The bench is measuring
                // write + fsync, not alloc churn — alloc cost is
                // identical between backends and dominated by
                // the I/O side.
                dev.free(&ext).unwrap();
            });
        });

        // Candidate: UringFileBackedDevice (only when the io_uring
        // feature is on AND the host kernel supports the ops we
        // need; UringFileBackedDevice::try_init returns Err in the
        // unsupported case and the bench skips below).
        #[cfg(feature = "io_uring")]
        {
            group.bench_with_input(BenchmarkId::new("uring", shape), &payload, |b, payload| {
                let dir = tempdir().unwrap();
                let dev = match UringFileBackedDevice::try_init(
                    &dir.path().join("bench.dev"),
                    DEV_SIZE,
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!(
                            "uring init failed ({e}); skipping uring half. \
                                 Acceptance gate cannot be evaluated on this host.",
                        );
                        return;
                    }
                };
                b.iter(|| {
                    let ext = dev.alloc(payload.len() as u64).unwrap();
                    dev.write(&ext, black_box(payload)).unwrap();
                    dev.sync().unwrap();
                    dev.free(&ext).unwrap();
                });
            });
        }
    }

    group.finish();
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("read");
    group.sample_size(20);

    for &shape in SHAPES {
        let payload = vec![0x5Au8; shape];

        // Baseline: FileBackedDevice.
        group.throughput(Throughput::Bytes(shape as u64));
        group.bench_with_input(BenchmarkId::new("file", shape), &payload, |b, payload| {
            let dir = tempdir().unwrap();
            let dev = FileBackedDevice::init(&dir.path().join("bench.dev"), DEV_SIZE).unwrap();
            let ext = dev.alloc(payload.len() as u64).unwrap();
            dev.write(&ext, payload).unwrap();
            dev.sync().unwrap();
            b.iter(|| {
                let got = dev.read(black_box(&ext)).unwrap();
                black_box(got);
            });
        });

        // Candidate: uring.
        #[cfg(feature = "io_uring")]
        {
            group.bench_with_input(BenchmarkId::new("uring", shape), &payload, |b, payload| {
                let dir = tempdir().unwrap();
                let dev = match UringFileBackedDevice::try_init(
                    &dir.path().join("bench.dev"),
                    DEV_SIZE,
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("uring init failed ({e}); skipping uring half.",);
                        return;
                    }
                };
                let ext = dev.alloc(payload.len() as u64).unwrap();
                dev.write(&ext, payload).unwrap();
                dev.sync().unwrap();
                b.iter(|| {
                    let got = dev.read(black_box(&ext)).unwrap();
                    black_box(got);
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_write_fsync, bench_read);
criterion_main!(benches);
