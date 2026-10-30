import json, sys, array
sys.path.insert(0, "/opt/mlat-server")
import numpy as np
from mlat import solver
r = json.loads(open(sys.argv[1]).read().splitlines()[int(sys.argv[2])])
pr = [(array.array('d', p), a, b) for p, a, b in r["pr"]]
for v in solver._residuals(np.array(r["x0"]), pr, r["alt"], r["alt_err"]):
    print("%.17e" % v)
