# Test-only launcher: runs the unmodified mlat-server while logging the inputs and
# outputs of its numerical core (leastsq, solver.solve, normalize2, Kalman update)
# as JSON lines under /rec, to serve as golden data for the Rust port.
import json, math, os, sys, time

sys.path.insert(0, "/opt/mlat-server")
REC = os.environ.get("MLAT_RECORD_DIR", "/rec")
os.makedirs(REC, exist_ok=True)


def fl(x):
    """JSON-safe float: non-finite values become strings."""
    x = float(x)
    return x if math.isfinite(x) else str(x)


def arr(a):
    return None if a is None else [fl(v) for v in a]


def mat(a):
    return None if a is None else [[fl(v) for v in row] for row in a]


def opener(name):
    return open(os.path.join(REC, name), "w", buffering=1)


f_ls, f_solve, f_norm, f_kal = (opener(n) for n in ("leastsq.jsonl", "solve.jsonl", "normalize2.jsonl", "kalman.jsonl"))

# ---- scipy.optimize.leastsq: the Levenberg-Marquardt call itself -------------
import scipy.optimize as so

_orig_leastsq = so.leastsq


def leastsq(func, x0, args=(), **kw):
    res = _orig_leastsq(func, x0, args=args, **kw)
    try:
        pr, alt, alt_err = args
        x, cov, info, _, ler = res
        f_ls.write(json.dumps({
            "x0": arr(x0),
            "pr": [[arr(p), fl(r), fl(e)] for p, r, e in pr],
            "alt": None if alt is None else fl(alt),
            "alt_err": None if alt_err is None else fl(alt_err),
            "maxfev": kw.get("maxfev"),
            "x": arr(x), "cov": mat(cov), "nfev": int(info["nfev"]), "info": int(ler),
        }) + "\n")
    except Exception as e:  # logging must never disturb the server
        sys.stderr.write("record leastsq: %r\n" % (e,))
    return res


so.leastsq = leastsq

# ---- solver.solve --------------------------------------------------------------
from mlat import solver

_orig_solve = solver.solve


def solve(measurements, altitude, altitude_error, initial_guess):
    res = _orig_solve(measurements, altitude, altitude_error, initial_guess)
    try:
        f_solve.write(json.dumps({
            "m": [[arr(r.position), fl(ts), fl(var)] for r, ts, var in measurements],
            "alt": None if altitude is None else fl(altitude),
            "alt_err": None if altitude_error is None else fl(altitude_error),
            "guess": arr(initial_guess),
            "out": None if res is None else [arr(res[0]), mat(res[1])],
        }) + "\n")
    except Exception as e:
        sys.stderr.write("record solve: %r\n" % (e,))
    return res


solver.solve = solve

# ---- clocktrack.normalize2 -------------------------------------------------------
from mlat import clocktrack

_orig_norm = clocktrack.normalize2
_norm_count = [0]
_norm_ctx = []   # normalize2 records made during the resolve being recorded


def normalize2(clocktracker, timestamp_map):
    now = time.time()
    recs = list(timestamp_map.keys())
    pairs = []
    for i, a in enumerate(recs):
        for b in recs[i + 1:]:
            lo, hi = (a, b) if a < b else (b, a)
            p = clocktracker.clock_pairs.get((lo, hi))
            if p is not None:
                pairs.append([lo.uid, hi.uid, bool(p.valid), fl(p.variance), fl(p.updated), p.n,
                              fl(p.factor), fl(p.i_factor), fl(p.base_avg), fl(p.peer_avg)])
    comps = _orig_norm(clocktracker, timestamp_map)
    if True:
        try:
            rec = ({
                "now": now,
                "in": [[r.uid, r.clock.freq, fl(r.clock.jitter), [[fl(t), fl(u)] for t, u in v]] for r, v in timestamp_map.items()],
                "pairs": pairs,
                "out": [{str(r.uid): [fl(var), [[fl(t), fl(u)] for t, u in ts]] for r, (var, ts) in c.items()} for c in comps],
            })
            _norm_ctx.append(rec)
            if _norm_count[0] < 8000:
                _norm_count[0] += 1
                f_norm.write(json.dumps(rec) + "\n")
        except Exception as e:
            sys.stderr.write("record normalize2: %r\n" % (e,))
    return comps


clocktrack.normalize2 = normalize2

# ---- Kalman filter -------------------------------------------------------------
from mlat import kalman

_orig_update = kalman.KalmanState.update
_tracked = set()


def update(self, position_time, measurements, altitude, altitude_error,
           leastsquares_position, leastsquares_cov, distinct, dof):
    if self.icao not in _tracked and len(_tracked) < 80:
        _tracked.add(self.icao)
    log = self.icao in _tracked
    res = _orig_update(self, position_time, measurements, altitude, altitude_error,
                       leastsquares_position, leastsquares_cov, distinct, dof)
    if log:
        try:
            f_kal.write(json.dumps({
                "icao": self.icao, "kind": type(self).__name__,
                "t": fl(position_time),
                "m": [[arr(r.position), fl(ts), fl(var)] for r, ts, var in measurements],
                "alt": None if altitude is None else fl(altitude),
                "alt_err": None if altitude_error is None else fl(altitude_error),
                "ls_pos": arr(leastsquares_position), "ls_cov": mat(leastsquares_cov),
                "distinct": distinct, "dof": dof,
                "ret": bool(res),
                "mean": arr(self._mean), "cov": mat(self._cov),
                "acquiring": bool(self._acquiring), "outliers": int(self._outliers),
                "last_update": fl(self.last_update), "valid": bool(self.valid),
                "heading": None if self.heading is None else fl(self.heading),
                "ground_speed": None if self.ground_speed is None else fl(self.ground_speed),
                "vertical_speed": None if self.vertical_speed is None else fl(self.vertical_speed),
                "position_error": None if self.position_error is None else fl(self.position_error),
                "velocity_error": None if self.velocity_error is None else fl(self.velocity_error),
            }) + "\n")
        except Exception as e:
            sys.stderr.write("record kalman: %r\n" % (e,))
    return res


kalman.KalmanState.update = update


# ---- MlatTracker._resolve: whole-function inputs and outputs ---------------------
import modes_cython.message as _M
from mlat import mlattrack

f_res = opener("resolve.jsonl")
_real_time = time.time


class _TimeShim:
    """Freezes time.time() for the duration of one recorded _resolve call, so the
    recorded 'now' is exactly what every function inside saw."""
    frozen = None

    def time(self):
        return _TimeShim.frozen if _TimeShim.frozen is not None else _real_time()

    def __getattr__(self, name):
        return getattr(time, name)


mlattrack.time = _TimeShim()
clocktrack.time = mlattrack.time


def _num(x):
    return None if x is None else fl(x)


def snap_ac(ac):
    k = ac.kalman
    return {
        "allow_mlat": bool(ac.allow_mlat), "seen": _num(ac.seen),
        "mmc": ac.mlat_message_count, "mrc": ac.mlat_result_count, "mkc": ac.mlat_kalman_count,
        "altitude": ac.altitude, "last_altitude_time": _num(ac.last_altitude_time),
        "alt_history": [[fl(t), a] for t, a in ac.alt_history],
        "vrate": ac.vrate, "vrate_time": _num(ac.vrate_time),
        "squawk": ac.squawk, "callsign": ac.callsign,
        "last_result_time": _num(ac.last_result_time),
        "last_result_position": arr(ac.last_result_position),
        "last_result_var": _num(ac.last_result_var),
        "last_result_dof": getattr(ac, "last_result_dof", None),
        "last_resolve_attempt": _num(ac.last_resolve_attempt),
        "last_force_mlat": _num(ac.last_force_mlat), "force_mlat": bool(ac.force_mlat),
        "kalman": {"mean": arr(k._mean), "cov": mat(k._cov), "acquiring": bool(k._acquiring),
                   "outliers": int(k._outliers), "last_update": _num(k.last_update), "valid": bool(k.valid)},
    }


def snap_kalman(k):
    return {"valid": bool(k.valid), "last_update": _num(k.last_update), "heading": _num(k.heading),
            "ground_speed": _num(k.ground_speed), "vertical_speed": _num(k.vertical_speed)}


_orig_resolve = mlattrack.MlatTracker._resolve
_res_calls = [0]
_res_logged = [0]


def _resolve(self, group):
    _res_calls[0] += 1
    copies = list(group.copies)
    # every call with 3+ copies, plus a thin sample of the rest
    if _res_logged[0] >= 40000 or (len(copies) < 3 and _res_calls[0] % 300):
        return _orig_resolve(self, group)
    _res_logged[0] += 1
    now = _real_time()
    _TimeShim.frozen = now
    captured, forwarded = [], []
    handler = lambda *a: captured.append(a)
    coord = self.coordinator
    orig_fwd = coord.forward_results
    try:
        decoded = _M.decode(group.message)
        address = decoded.address if decoded else None
        ac = self.tracker.aircraft.get(address) if address else None
        pre = snap_ac(ac) if ac is not None else None
        del _norm_ctx[:]
        coord.output_handlers.append(handler)

        def fwd(*a):
            forwarded.append(a)
            return orig_fwd(*a)

        coord.forward_results = fwd
        try:
            _orig_resolve(self, group)
        finally:
            coord.output_handlers.remove(handler)
            coord.forward_results = orig_fwd
        post = snap_ac(ac) if ac is not None else None
        uids = {}
        for r, t, u in copies:
            uids[r.uid] = [arr(r.position), r.clock.freq, fl(r.clock.jitter)]
        f_res.write(json.dumps({
            "now": now, "msg": bytes(group.message).hex(), "first_seen": fl(group.first_seen),
            "copies": [[r.uid, fl(t), fl(u)] for r, t, u in copies],
            "group_receivers": [r.uid for r in group.receivers],
            "receivers": {str(k): v for k, v in uids.items()},
            "pre": pre, "post": post, "norm": list(_norm_ctx),
            "emitted": [{"t": fl(a[0]), "addr": a[1], "ecef": arr(a[2]), "cov": mat(a[3]),
                         "receivers": [r.uid for r in a[4]], "distinct": a[5], "dof": a[6],
                         "kalman": snap_kalman(a[7]), "error": a[8]} for a in captured],
            "forwarded": [{"t": fl(a[0]), "addr": a[1], "n_receivers": len(a[4]), "dof": a[6],
                           "kalman": snap_kalman(a[7])} for a in forwarded],
        }) + "\n")
    except Exception as e:
        sys.stderr.write("record resolve: %r\n" % (e,))
        coord.forward_results = orig_fwd
    finally:
        _TimeShim.frozen = None


mlattrack.MlatTracker._resolve = _resolve

import runpy
sys.argv[0] = "/opt/mlat-server/mlat-server"
runpy.run_path("/opt/mlat-server/mlat-server", run_name="__main__")
