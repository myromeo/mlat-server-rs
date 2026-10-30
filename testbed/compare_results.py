#!/usr/bin/env python3
"""Compare the positions two runs (or a run and a live capture) produced.

Each source is either a capture event file from `mlat-capture extract` (the
live server's own replies) or a `results.jsonl` written by `mlat-capture
replay`. Both use the same time origin (first packet of the capture), so a
position is matched by aircraft address and arrival time at the client.

    compare_results.py A B [--window 1.5]
"""
import argparse, json, math, statistics, collections


def load(path):
    """-> {(addr, stamp): (t_arrival, ecef, n, nd)} one entry per unique result."""
    out = {}
    with open(path) as f:
        for line in f:
            if line.startswith('{"ev":') and not line.startswith('{"ev":"s2c"'):
                continue
            e = json.loads(line)
            try:
                msg = json.loads(e["l"])
            except (KeyError, ValueError):
                continue
            r = msg.get("result")
            if not r or "ecef" not in r:
                continue
            key = (r["addr"], r["@"])
            if key not in out or e["t"] < out[key][0]:
                out[key] = (e["t"], tuple(r["ecef"]), r.get("n"), r.get("nd"))
    return out


def by_addr(res):
    d = collections.defaultdict(list)
    for (addr, _), v in res.items():
        d[addr].append(v)
    for v in d.values():
        v.sort()
    return d


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def match(a, b, window):
    """Nearest-in-time pairing per aircraft; returns list of 3D distances (m)."""
    A, B = by_addr(a), by_addr(b)
    dists, matched = [], 0
    for addr, pa in A.items():
        pb = B.get(addr)
        if not pb:
            continue
        for t, pos, *_ in pa:
            best = min(pb, key=lambda x: abs(x[0] - t))
            if abs(best[0] - t) <= window:
                matched += 1
                dists.append(math.dist(pos, best[1]))
    return dists, matched


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--window", type=float, default=1.5, help="max arrival-time gap for a match (s)")
    args = ap.parse_args()
    a, b = load(args.a), load(args.b)
    A, B = set(by_addr(a)), set(by_addr(b))
    print(f"A: {len(a)} positions, {len(A)} aircraft    B: {len(b)} positions, {len(B)} aircraft")
    print(f"aircraft in both: {len(A & B)}   only A: {len(A - B)}   only B: {len(B - A)}")
    for name, x, y in (("A->B", a, b), ("B->A", b, a)):
        d, m = match(x, y, args.window)
        if not d:
            print(f"{name}: no matches")
            continue
        print(f"{name}: matched {m}/{len(x)} ({100*m/len(x):.0f}%)  "
              f"distance m: median {statistics.median(d):.0f}  p90 {pct(d,.9):.0f}  p99 {pct(d,.99):.0f}  max {max(d):.0f}")


if __name__ == "__main__":
    main()
