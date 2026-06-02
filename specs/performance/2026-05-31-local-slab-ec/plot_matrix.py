#!/usr/bin/env python3
"""
ADR-048 local matrix plotter — consumes results.json and produces
PNG charts under the same directory.

Two figures:
  1. throughput_by_cell.png — ops/s grouped by (protocol, shape, size)
     with side-by-side bars for baseline vs slab-EC.
  2. latency_by_cell.png    — p99 µs grouped the same way.

A third figure if the slab-EC delta is non-trivial:
  3. delta_pct.png         — % change vs baseline per cell.

Usage:
    python3 plot_matrix.py [--results results.json]
"""
import argparse
import json
import sys
from pathlib import Path

try:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import numpy as np
except ImportError as e:
    print(f"ERROR: matplotlib unavailable ({e}); install via `pip install matplotlib`.")
    sys.exit(2)


def short_size(n: int) -> str:
    if n >= 1 << 20:
        return f"{n >> 20}MiB"
    if n >= 1 << 10:
        return f"{n >> 10}KiB"
    return f"{n}B"


def cell_key(c: dict) -> str:
    return f"{c['protocol']}-{c['shape']}-{short_size(c['object_size'])}"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser()
    p.add_argument(
        "--results",
        type=Path,
        default=Path(__file__).parent / "results" / "results.json",
        help="path to results.json (default ./results/results.json)",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()
    if not args.results.exists():
        print(f"ERROR: {args.results} not found. Run run_matrix.py first.")
        return 2
    payload = json.loads(args.results.read_text())
    cells = payload["cells"]
    out_dir = args.results.parent

    # Group: cell_key → {pool_type → cell dict}
    grouped: dict[str, dict[str, dict]] = {}
    for c in cells:
        if not c.get("ok"):
            continue
        grouped.setdefault(cell_key(c), {})[c["pool_type"]] = c
    if not grouped:
        print("ERROR: no successful cells in results.json")
        return 2

    keys = sorted(grouped.keys())
    pool_types = sorted({c["pool_type"] for c in cells})
    base_color = {"baseline": "#1f77b4", "slabec": "#ff7f0e"}

    # --- Throughput chart -------------------------------------------------
    fig, ax = plt.subplots(figsize=(max(8, 0.45 * len(keys)), 5))
    x = np.arange(len(keys))
    width = 0.8 / max(1, len(pool_types))
    for i, pool_type in enumerate(pool_types):
        ys = [grouped[k].get(pool_type, {}).get("ops_per_s", 0) for k in keys]
        ax.bar(x + i * width, ys, width, label=pool_type, color=base_color.get(pool_type))
    ax.set_xticks(x + width * (len(pool_types) - 1) / 2)
    ax.set_xticklabels(keys, rotation=70, ha="right", fontsize=8)
    ax.set_ylabel("Throughput (ops/sec)")
    ax.set_title(
        f"ADR-048 local matrix — throughput "
        f"(nodes={payload['args']['nodes']}, "
        f"duration={payload['args']['duration']}s, "
        f"concurrency={payload['args']['concurrency']})"
    )
    ax.legend()
    ax.grid(axis="y", alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_dir / "throughput_by_cell.png", dpi=140)
    plt.close(fig)
    print(f"wrote {out_dir / 'throughput_by_cell.png'}")

    # --- p99 latency chart ------------------------------------------------
    fig, ax = plt.subplots(figsize=(max(8, 0.45 * len(keys)), 5))
    for i, pool_type in enumerate(pool_types):
        ys = [grouped[k].get(pool_type, {}).get("p99_us", 0) / 1000.0 for k in keys]
        ax.bar(x + i * width, ys, width, label=pool_type, color=base_color.get(pool_type))
    ax.set_xticks(x + width * (len(pool_types) - 1) / 2)
    ax.set_xticklabels(keys, rotation=70, ha="right", fontsize=8)
    ax.set_ylabel("p99 latency (ms)")
    ax.set_title("ADR-048 local matrix — p99 latency")
    ax.legend()
    ax.grid(axis="y", alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_dir / "latency_by_cell.png", dpi=140)
    plt.close(fig)
    print(f"wrote {out_dir / 'latency_by_cell.png'}")

    # --- % delta vs baseline ---------------------------------------------
    if "baseline" in pool_types and any(pt != "baseline" for pt in pool_types):
        non_base = [pt for pt in pool_types if pt != "baseline"]
        fig, ax = plt.subplots(figsize=(max(8, 0.45 * len(keys)), 5))
        width = 0.8 / max(1, len(non_base))
        for i, pool_type in enumerate(non_base):
            ys = []
            for k in keys:
                bl = grouped[k].get("baseline", {}).get("ops_per_s")
                sl = grouped[k].get(pool_type, {}).get("ops_per_s")
                if bl and sl:
                    ys.append(100.0 * (sl - bl) / bl)
                else:
                    ys.append(0.0)
            colors = ["#2ca02c" if v >= 0 else "#d62728" for v in ys]
            ax.bar(x + i * width, ys, width, label=f"{pool_type} vs baseline", color=colors)
        ax.axhline(0, color="black", linewidth=0.5)
        ax.set_xticks(x + width * (len(non_base) - 1) / 2)
        ax.set_xticklabels(keys, rotation=70, ha="right", fontsize=8)
        ax.set_ylabel("% Δ ops/s vs baseline")
        ax.set_title("ADR-048 — slab-EC throughput delta vs baseline (green = improvement)")
        ax.legend()
        ax.grid(axis="y", alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / "delta_pct.png", dpi=140)
        plt.close(fig)
        print(f"wrote {out_dir / 'delta_pct.png'}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
