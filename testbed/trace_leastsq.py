#!/usr/bin/env python3
"""Print every evaluation point of scipy's leastsq for one recorded problem."""
import json, sys
sys.path.insert(0, "/opt/mlat-server")
import scipy.optimize
from mlat import solver

line = open(sys.argv[1]).read().splitlines()[int(sys.argv[2])]
r = json.loads(line)
pr = [(tuple(p), a, b) for p, a, b in r["pr"]]
k = [0]


def f(x, *args):
    res = solver._residuals(x, *args)
    print(k[0], " ".join("%.17e" % v for v in x), "|", "%.17e" % sum(v * v for v in res))
    k[0] += 1
    return res


x, cov, info, _, ler = scipy.optimize.leastsq(f, r["x0"], args=(pr, r["alt"], r["alt_err"]), full_output=True, maxfev=r["maxfev"])
print("final info=%d nfev=%d" % (ler, info["nfev"]))
