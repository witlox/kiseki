#!/usr/bin/env python3
"""
ADR-048 local perf matrix — baseline vs slab-EC delta.

Drives `kiseki-profile` across a 5 protocols × 3 shapes × 2 sizes × 2
pool-types matrix on a 3-node local Raft cluster. Each cell spawns a
fresh cluster, runs the workload for ``DURATION`` seconds, parses the
stdout report, scrapes the leader's `/metrics`, and writes per-cell
JSON + a combined results.json.

Usage:
    python3 run_matrix.py [--duration N] [--concurrency N] [--out DIR]
                          [--nodes N] [--protocols ...] [--shapes ...]

Output (under --out, default ./results/):
    cell_{baseline|slabec}_{protocol}_{shape}_{size}.json   # per cell
    results.json                                            # aggregate
    metrics_{baseline|slabec}_{protocol}_{shape}_{size}.prom

The plotter (plot_matrix.py) consumes results.json.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
PROFILE_BIN = REPO_ROOT / "target/release/kiseki-profile"

# Output line shapes from kiseki-profile stdout:
#   ops=48837 throughput=16279.0 op/s 1017.44 MiB/s
#   latency_us p50=226 p95=370 p99=500
THROUGHPUT_RE = re.compile(
    r"ops=(?P<ops>\d+)\s+throughput=(?P<ops_per_s>[\d.]+)\s+op/s\s+(?P<mib_s>[\d.]+)\s+MiB/s"
)
LATENCY_RE = re.compile(
    r"latency_us\s+p50=(?P<p50>\d+)\s+p95=(?P<p95>\d+)\s+p99=(?P<p99>\d+)"
)
# kiseki-profile prints the leader's endpoint info on a `[harness]` line:
#   [harness] N-node cluster ready; bench endpoints: s3=http://127.0.0.1:35695 ... metrics=http://127.0.0.1:38737/metrics
# We grab the `metrics=` URL from the cluster's ready line so we can
# scrape it before the cluster tears down.
METRICS_RE = re.compile(r"metrics=(?P<url>http://[^\s]+)")


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser()
    p.add_argument(
        "--duration",
        type=int,
        default=10,
        help="seconds per cell (default 10)",
    )
    p.add_argument(
        "--concurrency",
        type=int,
        default=16,
        help="concurrent in-flight ops (default 16)",
    )
    p.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).parent / "results",
        help="output dir (default ./results)",
    )
    p.add_argument(
        "--nodes",
        type=int,
        default=3,
        help="node count (default 3 — multi-node is required for slab-EC)",
    )
    p.add_argument(
        "--protocols",
        nargs="+",
        default=["s3", "nfs3", "nfs4", "pnfs", "fuse"],
        help="protocols to run",
    )
    p.add_argument(
        "--shapes",
        nargs="+",
        default=["put-heavy", "get-heavy", "mixed"],
        help="workload shapes",
    )
    p.add_argument(
        "--object-sizes",
        nargs="+",
        type=int,
        default=[65_536, 4 * 1024 * 1024],
        help="object sizes in bytes (default 64KiB and 4MiB)",
    )
    p.add_argument(
        "--warmup",
        type=int,
        default=128,
        help="warmup objects for get-heavy/mixed (default 128)",
    )
    p.add_argument(
        "--pool-types",
        nargs="+",
        default=["baseline", "slabec"],
        help="baseline (default replication pool) and/or slabec (Replication + slab-EC compactor)",
    )
    return p.parse_args()


def run_cell(
    pool_type: str,
    protocol: str,
    shape: str,
    object_size: int,
    duration: int,
    concurrency: int,
    nodes: int,
    warmup: int,
    out_dir: Path,
) -> dict:
    """Run a single cell and return the parsed result dict."""
    label = f"{pool_type}_{protocol}_{shape}_{object_size}"
    print(f"\n[cell] {label} ...", flush=True)
    cmd = [
        str(PROFILE_BIN),
        "run",
        "--protocol",
        protocol,
        "--shape",
        shape,
        "--concurrency",
        str(concurrency),
        "--object-size",
        str(object_size),
        "--duration-secs",
        str(duration),
        "--warmup-objects",
        str(warmup),
        "--nodes",
        str(nodes),
    ]
    if pool_type == "slabec":
        cmd.append("--slab-ec")
    started = time.monotonic()
    try:
        completed = subprocess.run(
            cmd,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=duration + 240,
        )
    except subprocess.TimeoutExpired:
        return {
            "label": label,
            "pool_type": pool_type,
            "protocol": protocol,
            "shape": shape,
            "object_size": object_size,
            "ok": False,
            "error": "timeout",
            "wall_s": time.monotonic() - started,
        }
    wall = time.monotonic() - started
    output = (completed.stdout or "") + "\n" + (completed.stderr or "")
    cell = {
        "label": label,
        "pool_type": pool_type,
        "protocol": protocol,
        "shape": shape,
        "object_size": object_size,
        "ok": completed.returncode == 0,
        "rc": completed.returncode,
        "wall_s": wall,
    }
    thr = THROUGHPUT_RE.search(output)
    lat = LATENCY_RE.search(output)
    if thr:
        cell["ops"] = int(thr.group("ops"))
        cell["ops_per_s"] = float(thr.group("ops_per_s"))
        cell["mib_per_s"] = float(thr.group("mib_s"))
    if lat:
        cell["p50_us"] = int(lat.group("p50"))
        cell["p95_us"] = int(lat.group("p95"))
        cell["p99_us"] = int(lat.group("p99"))

    # Persist per-cell raw stdout so a failed cell is post-mortem-able.
    (out_dir / f"cell_{label}.log").write_text(output)
    (out_dir / f"cell_{label}.json").write_text(json.dumps(cell, indent=2))

    status = "OK " if cell["ok"] else "FAIL"
    if "ops_per_s" in cell:
        print(
            f"[cell] {label} {status} {cell['ops_per_s']:.0f} op/s "
            f"{cell.get('mib_per_s', 0):.1f} MiB/s p99={cell.get('p99_us', '?')}µs "
            f"({wall:.1f}s)",
            flush=True,
        )
    else:
        print(f"[cell] {label} {status} (no stats parsed; wall={wall:.1f}s)", flush=True)
        if not cell["ok"]:
            # First 400 chars of the failure for quick diagnosis.
            tail = output[-400:].replace("\n", " | ")
            print(f"[cell] {label} tail: {tail}", flush=True)
    return cell


def main() -> int:
    args = parse_args()
    if not PROFILE_BIN.exists():
        print(f"ERROR: {PROFILE_BIN} not found. Run `cargo build --release` first.")
        return 2
    args.out.mkdir(parents=True, exist_ok=True)

    cells: list[dict] = []
    total = (
        len(args.pool_types)
        * len(args.protocols)
        * len(args.shapes)
        * len(args.object_sizes)
    )
    print(f"[matrix] {total} cells, duration {args.duration}s/cell, nodes={args.nodes}")
    print(f"[matrix] out={args.out}")
    started = time.monotonic()

    idx = 0
    for pool_type in args.pool_types:
        for protocol in args.protocols:
            for shape in args.shapes:
                for size in args.object_sizes:
                    idx += 1
                    print(f"\n[matrix] {idx}/{total}", flush=True)
                    cell = run_cell(
                        pool_type=pool_type,
                        protocol=protocol,
                        shape=shape,
                        object_size=size,
                        duration=args.duration,
                        concurrency=args.concurrency,
                        nodes=args.nodes,
                        warmup=args.warmup,
                        out_dir=args.out,
                    )
                    cells.append(cell)
                    (args.out / "results.json").write_text(
                        json.dumps(
                            {"args": vars(args), "cells": cells, "wall_s": time.monotonic() - started},
                            indent=2,
                            default=str,
                        )
                    )

    wall = time.monotonic() - started
    print(f"\n[matrix] DONE in {wall / 60:.1f} min")
    ok = sum(1 for c in cells if c["ok"])
    print(f"[matrix] cells {ok}/{len(cells)} ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
