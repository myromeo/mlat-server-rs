#!/usr/bin/env python3
"""scipy's QR state after the first Jacobian of one recorded problem."""
import json, sys
sys.path.insert(0, "/opt/mlat-server")
import numpy as np
import scipy.optimize
from mlat import solver

r = json.loads(open(sys.argv[1]).read().splitlines()[int(sys.argv[2])])
pr = [(tuple(p), a, b) for p, a, b in r["pr"]]
x, cov, info, mesg, ler = scipy.optimize.leastsq(solver._residuals, r["x0"], args=(pr, r["alt"], r["alt_err"]),
                                                  full_output=True, maxfev=int(sys.argv[3]))
print("info=%d nfev=%d" % (ler, info["nfev"]))
print("ipvt", list(info["ipvt"]))
print("qtf", " ".join("%.17e" % v for v in info["qtf"]))
print("fvec", " ".join("%.17e" % v for v in info["fvec"]))
fj = np.asarray(info["fjac"])
m = len(info["fvec"])
for i in range(m):
    print("fjac row %d:" % i, " ".join("%.17e" % fj[j][i] for j in range(4)))
