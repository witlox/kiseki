#!/usr/bin/env bash
# Transport benchmark runner for lab hardware.
#
# Detects available fabric transports, runs benchmarks, and writes
# results to specs/validation/transport-benchmarks.md.
#
# Usage:
#   ./tests/hw/run_transport_bench.sh              # auto-detect + run
#   ./tests/hw/run_transport_bench.sh --tcp-only    # TCP only (CI safe)
#   ./tests/hw/run_transport_bench.sh --detect      # detect only, don't run

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OUTPUT_DIR="${REPO_ROOT}/specs/validation"
OUTPUT_FILE="${OUTPUT_DIR}/transport-benchmarks.md"
DATE=$(date +%Y-%m-%d)

# Colors.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }

# ---------------------------------------------------------------------------
# Hardware detection
# ---------------------------------------------------------------------------

detect_transports() {
    local transports=()

    # CXI (Slingshot).
    if [ -d /sys/class/cxi ]; then
        local cxi_count
        cxi_count=$(ls -1 /sys/class/cxi/ 2>/dev/null | wc -l)
        if [ "$cxi_count" -gt 0 ]; then
            info "CXI devices found: $cxi_count"
            transports+=("cxi")
        fi
    fi

    # InfiniBand / RoCE.
    if [ -d /sys/class/infiniband ]; then
        for dev in /sys/class/infiniband/*/; do
            local devname
            devname=$(basename "$dev")
            local link_layer
            link_layer=$(cat "${dev}ports/1/link_layer" 2>/dev/null || echo "unknown")
            if [ "$link_layer" = "Ethernet" ]; then
                info "RoCEv2 device found: $devname"
                transports+=("roce")
            elif [ "$link_layer" = "InfiniBand" ]; then
                info "InfiniBand device found: $devname"
                transports+=("ib")
            fi
        done
    fi

    # NVIDIA GPU (GPUDirect Storage).
    if command -v nvidia-smi &>/dev/null; then
        local gpu_count
        gpu_count=$(nvidia-smi -L 2>/dev/null | wc -l)
        if [ "$gpu_count" -gt 0 ]; then
            info "NVIDIA GPUs found: $gpu_count"
            transports+=("gpu-nvidia")
        fi
    fi

    # AMD GPU (ROCm).
    if [ -d /sys/class/kfd/kfd/topology/nodes ]; then
        local gpu_count
        gpu_count=$(ls -1d /sys/class/kfd/kfd/topology/nodes/*/properties 2>/dev/null | wc -l)
        if [ "$gpu_count" -gt 0 ]; then
            info "AMD GPUs found (KFD nodes): $gpu_count"
            transports+=("gpu-amd")
        fi
    fi

    # NVMe devices.
    if [ -d /sys/class/nvme ]; then
        local nvme_count
        nvme_count=$(ls -1 /sys/class/nvme/ 2>/dev/null | wc -l)
        if [ "$nvme_count" -gt 0 ]; then
            info "NVMe devices found: $nvme_count"
            transports+=("nvme")
        fi
    fi

    # NUMA topology.
    if [ -d /sys/devices/system/node ]; then
        local numa_count
        numa_count=$(ls -1d /sys/devices/system/node/node* 2>/dev/null | wc -l)
        info "NUMA nodes: $numa_count"
    fi

    # TCP is always available.
    transports+=("tcp")

    echo "${transports[@]}"
}

# ---------------------------------------------------------------------------
# NVMe latency benchmark
# ---------------------------------------------------------------------------

bench_nvme_latency() {
    local device="${1:-/dev/nvme0n1}"
    info "NVMe latency benchmark on $device"

    if ! command -v fio &>/dev/null; then
        warn "fio not installed — skipping NVMe latency bench"
        echo "| NVMe 4KB write | < 20 µs | _fio not installed_ | SKIP |"
        return
    fi

    if [ ! -b "$device" ]; then
        warn "Device $device not found — skipping"
        echo "| NVMe 4KB write | < 20 µs | _device not found_ | SKIP |"
        return
    fi

    # 4KB random write, direct I/O, 1 thread, 1000 ops.
    local result
    result=$(fio --name=latency_test \
        --filename="$device" \
        --ioengine=io_uring \
        --direct=1 \
        --rw=randwrite \
        --bs=4k \
        --iodepth=1 \
        --numjobs=1 \
        --size=4m \
        --runtime=10 \
        --time_based \
        --output-format=json \
        2>/dev/null)

    local lat_ns
    lat_ns=$(echo "$result" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(int(data['jobs'][0]['write']['lat_ns']['mean']))
" 2>/dev/null || echo "0")

    local lat_us=$((lat_ns / 1000))
    local status="PASS"
    if [ "$lat_us" -gt 20 ]; then
        status="FAIL"
    fi

    echo "| NVMe 4KB write | < 20 µs | ${lat_us} µs | ${status} |"
}

# ---------------------------------------------------------------------------
# EC encode benchmark
# ---------------------------------------------------------------------------

bench_ec_encode() {
    info "EC encode overhead benchmark"

    # Build and run a quick EC encode benchmark.
    local output
    output=$(cd "$REPO_ROOT" && cargo run --release --example ec_bench 2>/dev/null || echo "SKIP")

    if [ "$output" = "SKIP" ]; then
        echo "| EC 4+2 overhead | < 5% CPU | _ec_bench not available_ | SKIP |"
        return
    fi

    echo "$output"
}

# ---------------------------------------------------------------------------
# HDD sequential throughput
# ---------------------------------------------------------------------------

bench_hdd_throughput() {
    local device="${1:-}"
    if [ -z "$device" ]; then
        warn "No HDD device specified — skipping"
        echo "| HDD seq read | > 200 MB/s | _no device specified_ | SKIP |"
        return
    fi

    info "HDD sequential throughput on $device"

    if ! command -v fio &>/dev/null; then
        warn "fio not installed — skipping HDD bench"
        echo "| HDD seq read | > 200 MB/s | _fio not installed_ | SKIP |"
        return
    fi

    local result
    result=$(fio --name=seq_read \
        --filename="$device" \
        --ioengine=libaio \
        --direct=1 \
        --rw=read \
        --bs=1m \
        --iodepth=32 \
        --numjobs=1 \
        --size=1g \
        --runtime=30 \
        --time_based \
        --output-format=json \
        2>/dev/null)

    local bw_kbps
    bw_kbps=$(echo "$result" | python3 -c "
import json, sys
data = json.load(sys.stdin)
print(int(data['jobs'][0]['read']['bw']))
" 2>/dev/null || echo "0")

    local bw_mbps=$((bw_kbps / 1024))
    local status="PASS"
    if [ "$bw_mbps" -lt 200 ]; then
        status="FAIL"
    fi

    echo "| HDD seq read | > 200 MB/s | ${bw_mbps} MB/s | ${status} |"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    local mode="full"
    local hdd_device=""

    while [[ $# -gt 0 ]]; do
        case $1 in
            --tcp-only)  mode="tcp-only"; shift ;;
            --detect)    mode="detect"; shift ;;
            --hdd)       hdd_device="$2"; shift 2 ;;
            *)           error "Unknown option: $1"; exit 1 ;;
        esac
    done

    echo "=== Kiseki Transport Benchmark ==="
    echo "Date: $DATE"
    echo ""

    # Detect hardware.
    info "Detecting available transports..."
    local transports
    transports=$(detect_transports)
    echo ""
    info "Available: $transports"
    echo ""

    if [ "$mode" = "detect" ]; then
        info "Detection only — exiting."
        exit 0
    fi

    # Build the bench binary.
    info "Building transport benchmark (release)..."
    cd "$REPO_ROOT"
    cargo build --release --example transport_bench 2>/dev/null || {
        warn "transport_bench example not found — running Rust bench inline"
    }

    # Create output directory.
    mkdir -p "$OUTPUT_DIR"

    # Run TCP benchmark (always).
    info "Running TCP loopback benchmark..."
    BENCH_OUTPUT="$OUTPUT_FILE" cargo run --release --example transport_bench 2>&1 | \
        grep -v "^\[" || true

    # Append hardware-specific results.
    {
        echo ""
        echo "## Hardware Validation (${DATE})"
        echo ""
        echo "| Assumption | Expected | Measured | Status |"
        echo "|------------|----------|----------|--------|"

        # NVMe latency.
        if echo "$transports" | grep -q "nvme"; then
            bench_nvme_latency
        else
            echo "| NVMe 4KB write | < 20 µs | _no NVMe_ | SKIP |"
        fi

        # EC encode.
        bench_ec_encode

        # HDD throughput.
        if [ -n "$hdd_device" ]; then
            bench_hdd_throughput "$hdd_device"
        else
            echo "| HDD seq read | > 200 MB/s | _no --hdd specified_ | SKIP |"
        fi

        # CXI latency.
        if echo "$transports" | grep -q "cxi"; then
            echo "| CXI 64B RTT | < 2 µs | _run CXI bench_ | PENDING |"
        else
            echo "| CXI 64B RTT | < 2 µs | _no CXI hardware_ | SKIP |"
        fi
    } >> "$OUTPUT_FILE"

    echo ""
    info "Results written to: $OUTPUT_FILE"
    info "Done."
}

main "$@"
