#!/usr/bin/env python3
"""Re-run recorded leastsq problems through the real scipy/MINPACK in this
image and write the results in the recorder's format. Used to get a reference
on a different CPU architecture (x86-64) than the one the recording ran on.

    rerun_leastsq.py IN.jsonl OUT.jsonl
"""
import json, math, sys
sys.path.insert(0, "/opt/mlat-server")
import scipy.optimize
from mlat import solver


def num(v):
    return float(v)


def fl(x):
    x = float(x)
    return x if math.isfinite(x) else str(x)


n = 0
with open(sys.argv[1]) as f, open(sys.argv[2], "w") as out:
    for line in f:
        try:
            r = json.loads(line)
        except ValueError:
            continue
        pr = [(tuple(num(c) for c in p), num(a), num(b)) for p, a, b in r["pr"]]
        alt = None if r["alt"] is None else num(r["alt"])
        alt_err = None if r["alt_err"] is None else num(r["alt_err"])
        x, cov, info, _, ler = scipy.optimize.leastsq(
            solver._residuals, [num(v) for v in r["x0"]], args=(pr, alt, alt_err),
            full_output=True, maxfev=r["maxfev"])
        r["x"] = [fl(v) for v in x]
        r["cov"] = None if cov is None else [[fl(v) for v in row] for row in cov]
        r["nfev"] = int(info["nfev"])
        r["info"] = int(ler)
        out.write(json.dumps(r) + "\n")
        n += 1
print("rerun", n)
