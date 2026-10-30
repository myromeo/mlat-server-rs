#!/usr/bin/env python3
"""Compare multilateration result streams, result by result.

    compare_pipeline.py A.jsonl B.jsonl            two pipeline outputs (py_pipeline_ref format)
    compare_pipeline.py A.jsonl --live capture.jsonl   A against the results the live server sent
                                                          to its clients (from `mlat-capture extract`)
Results are matched on aircraft address and result time (cluster_utc).
"""
import json, math, sys, collections


def load_pipeline(path):
    out = {}
    for line in open(path):
        r = json.loads(line)
        out[(r["addr"], round(r["t"], 3))] = r
    return out


def load_live(path):
    """Unique results the live server sent (one per aircraft+time; sent to several receivers)."""
    start = None
    out = {}
    for line in open(path):
        if line.startswith('{"ev":"meta"'):
            start = json.loads(line)["start_epoch"]
        if not line.startswith('{"ev":"s2c"'):
            continue
        e = json.loads(line)
        try:
            m = json.loads(e["l"])
        except ValueError:
            continue
        r = m.get("result")
        if not r or "ecef" not in r:
            continue
        out[(int(r["addr"], 16), round(r["@"] - start, 3))] = {"ecef": r["ecef"], "n": r["n"], "nd": r["nd"], "cov": r["cov"]}
    return out


def fuzzy_pairs(a, b, window):
    """Pair each A result with the nearest unused B result of the same aircraft within `window` s."""
    by = collections.defaultdict(list)
    for (addr, t), r in b.items():
        by[addr].append((t, (addr, t)))
    for v in by.values():
        v.sort()
    used = set()
    pairs = []
    for (addr, t) in sorted(a, key=lambda k: k[1]):
        best = None
        for tb, kb in by.get(addr, ()):
            if kb in used or abs(tb - t) > window:
                continue
            if best is None or abs(tb - t) < abs(best[0] - t):
                best = (tb, kb)
        if best:
            used.add(best[1])
            pairs.append(((addr, t), best[1]))
    return pairs


def pct(v, q):
    v = sorted(v)
    return v[min(len(v) - 1, int(len(v) * q))] if v else float("nan")


def main():
    a = load_pipeline(sys.argv[1])
    live = "--live" in sys.argv
    b = load_live(sys.argv[sys.argv.index("--live") + 1]) if live else load_pipeline(sys.argv[2])
    window = float(sys.argv[sys.argv.index("--window") + 1]) if "--window" in sys.argv else 0.0
    ka, kb = set(a), set(b)
    if window > 0:
        pairs = fuzzy_pairs(a, b, window)
        print(f"A: {len(a)} results   B: {len(b)} results   matched (same aircraft within {window}s): {len(pairs)}   unmatched A: {len(a) - len(pairs)}   unmatched B: {len(b) - len(pairs)}")
    else:
        pairs = [(k, k) for k in ka & kb]
        print(f"A: {len(a)} results   B: {len(b)} results   in both: {len(pairs)}   only A: {len(ka - kb)}   only B: {len(kb - ka)}")
    d = []
    bad = collections.Counter()
    for k1, k2 in pairs:
        x, y = a[k1], b[k2]
        d.append(math.dist(x["ecef"], y["ecef"]))
        if live:
            if x["distinct"] != y["nd"]:
                bad["distinct"] += 1
        else:
            for f in ("distinct", "dof", "error", "receivers", "kalman_valid"):
                if x[f] != y[f]:
                    bad[f] += 1
    if d:
        print(f"position difference over matched results (m): median {pct(d, .5):.3g}  p99 {pct(d, .99):.3g}  max {max(d):.3g}")
        print("other fields that differ:", dict(bad) or "none")
    if window <= 0:
        only_a = sorted(ka - kb)[:5]
        only_b = sorted(kb - ka)[:5]
        if only_a:
            print("  e.g. only in A:", only_a)
        if only_b:
            print("  e.g. only in B:", only_b)


if __name__ == "__main__":
    main()
