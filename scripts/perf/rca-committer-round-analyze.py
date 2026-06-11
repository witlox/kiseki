#!/usr/bin/env python3
"""GH #266 — window-accurate span averages from paired scrapes.

For each (phase, port): avg_us = (sum_b - sum_a) / (count_b - count_a)
per kiseki_hotpath_* label; prints fresh vs at-volume side by side,
plus throughput per window from the count deltas of the round spans.
"""
import re, sys, glob, collections

OUT = "/home/witlox/kiseki/.gcp-build/rca-266"

def parse(path):
    sums, counts = {}, {}
    for line in open(path):
        m = re.match(r'(kiseki_\w+)_(sum|count)\{[^}]*label="([^"]+)"[^}]*\}\s+([0-9.e+-]+)', line)
        if not m:
            m2 = re.match(r'(kiseki_\w+)_(sum|count)\s+([0-9.e+-]+)', line)
            continue
        name, kind, label, val = m.groups()
        key = label
        (sums if kind == "sum" else counts)[key] = float(val)
    return sums, counts

def window(phase):
    agg = collections.defaultdict(lambda: [0.0, 0.0])  # label -> [dsum, dcount]
    for fa in sorted(glob.glob(f"{OUT}/{phase}-a-*.txt") + glob.glob(f"{OUT}/{phase}-a*.txt")):
        fb = fa.replace(f"{phase}-a", f"{phase}-b")
        try:
            sa, ca = parse(fa); sb, cb = parse(fb)
        except FileNotFoundError:
            continue
        for k in sb:
            ds = sb.get(k, 0) - sa.get(k, 0)
            dc = cb.get(k, 0) - ca.get(k, 0)
            if dc > 0:
                agg[k][0] += ds; agg[k][1] += dc
    return {k: (v[0] / v[1] * 1e6, v[1]) for k, v in agg.items() if v[1] > 0}

fresh, vol = window("fresh"), window("vol")
labels = sorted(set(fresh) | set(vol), key=lambda k: -(vol.get(k, (0, 0))[0]))
print(f"{'span':<32}{'fresh µs':>12}{'fresh n':>10}{'vol µs':>12}{'vol n':>10}{'vol/fresh':>10}")
for k in labels:
    f = fresh.get(k); v = vol.get(k)
    r = f"{v[0]/f[0]:.1f}x" if f and v and f[0] > 0 else "-"
    print(f"{k:<32}{f[0] if f else 0:>12.1f}{int(f[1]) if f else 0:>10}{v[0] if v else 0:>12.1f}{int(v[1]) if v else 0:>10}{r:>10}")
