#!/usr/bin/env python3
"""Replay recorded Kalman update sequences through the real Python filter with
the altitude removed, to get a reference for the no-altitude observation model.

    rerun_kalman.py IN.jsonl OUT.jsonl
"""
import json, math, sys
sys.path.insert(0, "/opt/mlat-server")
import numpy
from mlat import kalman


def fl(x):
    x = float(x)
    return x if math.isfinite(x) else str(x)


def arr(a):
    return None if a is None else [fl(v) for v in a]


def mat(a):
    return None if a is None else [[fl(v) for v in row] for row in a]


class Rx:
    def __init__(self, pos):
        self.position = pos


by = {}
for line in open(sys.argv[1]):
    try:
        r = json.loads(line)
    except ValueError:
        continue
    by.setdefault(r["icao"], []).append(r)

n = 0
with open(sys.argv[2], "w") as out:
    for icao, recs in by.items():
        k = kalman.KalmanStateCA(icao)
        for r in recs:
            meas = [(Rx(numpy.array(p)), float(t), float(v)) for p, t, v in r["m"]]
            ret = k.update(float(r["t"]), meas, None, None, numpy.array([float(v) for v in r["ls_pos"]]),
                           numpy.array([[float(v) for v in row] for row in r["ls_cov"]]), r["distinct"], r["dof"])
            out.write(json.dumps({
                "icao": icao, "kind": "KalmanStateCA", "t": r["t"], "m": r["m"], "alt": None, "alt_err": None,
                "ls_pos": r["ls_pos"], "ls_cov": r["ls_cov"], "distinct": r["distinct"], "dof": r["dof"],
                "ret": bool(ret), "mean": arr(k._mean), "cov": mat(k._cov), "acquiring": bool(k._acquiring),
                "outliers": int(k._outliers), "last_update": fl(k.last_update), "valid": bool(k.valid),
                "heading": None if k.heading is None else fl(k.heading),
                "ground_speed": None if k.ground_speed is None else fl(k.ground_speed),
                "vertical_speed": None if k.vertical_speed is None else fl(k.vertical_speed),
                "position_error": None if k.position_error is None else fl(k.position_error),
                "velocity_error": None if k.velocity_error is None else fl(k.velocity_error),
            }) + "\n")
            n += 1
print("replayed", n)
