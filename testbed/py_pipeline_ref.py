#!/usr/bin/env python3
"""Deterministic reference for the WHOLE Python mlat-server pipeline.

Runs the real Coordinator (sync, tracking, interest selection, mlat grouping and
resolve, the 15 s health evaluation) on a capture with a virtual clock and a
virtual-time scheduler, no sockets. Every multilateration result the server
would hand to its output handlers is written as one JSON line, with the exact
virtual time, so a re-implementation can be compared result by result.

    docker run --rm --entrypoint python3 -v ~/Downloads:/data -v $PWD/testbed:/h \
        mlat-baseline:nofma /h/py_pipeline_ref.py /data/capture.jsonl /data/pipeline-golden.jsonl \
        [--max-secs N]

Determinism: `random` is seeded; the per-aircraft forced-mlat phase, which the
server draws at random, is replaced by a hash of the ICAO address.
"""
import heapq
import json
import math
import random
import re
import sys
import tempfile
import time

sys.path.insert(0, "/opt/mlat-server")

_real_process_time = time.process_time
_vnow = [0.0]
time.time = lambda: _vnow[0]

from mlat import coordinator as coordinator_mod  # noqa: E402
from mlat import tracker as tracker_mod  # noqa: E402

random.seed(12345)


class _Handle:
    def __init__(self, entry):
        self.entry = entry

    def cancel(self):
        self.entry[2] = None


class Scheduler:
    """A virtual-time stand-in for the asyncio loop."""

    def __init__(self):
        self.heap = []
        self.seq = 0

    def _push(self, when, cb, args):
        self.seq += 1
        entry = [when, self.seq, cb, args]
        heapq.heappush(self.heap, entry)
        return _Handle(entry)

    def call_later(self, delay, cb, *args):
        return self._push(_vnow[0] + delay, (lambda e=None: cb(*args)), ())

    def call_soon(self, cb, *args):
        return self._push(_vnow[0], (lambda e=None: cb(*args)), ())

    def add_signal_handler(self, *a, **k):
        pass

    def remove_signal_handler(self, *a, **k):
        pass

    def run_until(self, t):
        """Run every timer due at or before virtual time t, in order."""
        while self.heap and self.heap[0][0] <= t:
            when, _, cb, _args = heapq.heappop(self.heap)
            if cb is None:
                continue
            _vnow[0] = when
            cb()


class StubConnection:
    source_ip = "0.0.0.0"
    source_port = "0"
    message_counter = 0

    def request_traffic(self, *a, **k):
        pass

    def send_stats(self, *a, **k):
        pass

    def report_mlat_position(self, *a, **k):
        pass


def fl(x):
    x = float(x)
    return x if math.isfinite(x) else str(x)


def load_events(path, max_secs):
    events = []
    start_epoch = None
    with open(path) as f:
        for line in f:
            if line.startswith('{"ev":"c2s"'):
                e = json.loads(line)
                if max_secs is not None and e["t"] > max_secs:
                    break
                l = e["l"]
                head = l[:16].lstrip("{ ")
                if not head.startswith(('"sync"', '"mlat"', '"seen"', '"lost"', '"input_', '"clock_', '"rate_')):
                    continue
                m = json.loads(l)
                c = e["c"]
                if "sync" in m:
                    s = m["sync"]
                    events.append((e["t"], "sync", (c, float(s["et"]), float(s["ot"]), bytes.fromhex(s["em"]), bytes.fromhex(s["om"]))))
                elif "mlat" in m:
                    s = m["mlat"]
                    events.append((e["t"], "mlat", (c, float(s["t"]), bytes.fromhex(s["m"]))))
                elif "seen" in m:
                    events.append((e["t"], "seen", (c, {int(x, 16) for x in m["seen"]})))
                elif "lost" in m:
                    events.append((e["t"], "lost", (c, {int(x, 16) for x in m["lost"]})))
                elif "rate_report" in m:
                    events.append((e["t"], "rate_report", (c, {int(k, 16): v for k, v in m["rate_report"].items()})))
                elif "input_connected" in m:
                    events.append((e["t"], "input_connected", (c,)))
                elif "input_disconnected" in m or "clock_reset" in m or "clock_jump" in m:
                    events.append((e["t"], "clock_reset", (c,)))
            elif line.startswith('{"ev":"hs"'):
                e = json.loads(line)
                if max_secs is None or e["t"] <= max_secs:
                    events.append((e["t"], "hs", (e["c"], json.loads(e["line"]))))
            elif line.startswith('{"ev":"close"'):
                e = json.loads(line)
                if max_secs is None or e["t"] <= max_secs:
                    events.append((e["t"], "close", (e["c"],)))
            elif line.startswith('{"ev":"meta"'):
                start_epoch = json.loads(line)["start_epoch"]
    return start_epoch, events


def main():
    argv = sys.argv[1:]
    max_secs = None
    if "--max-secs" in argv:
        i = argv.index("--max-secs")
        max_secs = float(argv[i + 1])
        del argv[i:i + 2]
    capture, out_path = argv[0], argv[1]

    start_epoch, events = load_events(capture, max_secs)
    print(f"loaded {len(events)} events", file=sys.stderr)

    # the forced-mlat phase is random in the server; make it a function of the ICAO
    orig_init = tracker_mod.TrackedAircraft.__init__

    def det_init(self, icao, allow_mlat):
        orig_init(self, icao, allow_mlat)
        phase = ((icao * 2654435761) % (1 << 32)) / float(1 << 32)
        self.last_force_mlat = _vnow[0] - tracker_mod.FORCE_MLAT_INTERVAL * phase

    tracker_mod.TrackedAircraft.__init__ = det_init

    _vnow[0] = start_epoch
    sched = Scheduler()
    workdir = tempfile.mkdtemp()
    coord = coordinator_mod.Coordinator(work_dir=workdir, loop=sched, status_interval=-1, partition=(1, 1))

    out = open(out_path, "w")
    n_results = [0]

    def handler(cluster_utc, address, ecef, ecef_cov, receivers, distinct, dof, kalman, error):
        n_results[0] += 1
        out.write(json.dumps({
            "now": round(_vnow[0] - start_epoch, 6),
            "t": round(cluster_utc - start_epoch, 6), "addr": address,
            "ecef": [fl(v) for v in ecef],
            "cov": None if ecef_cov is None else [[fl(v) for v in row] for row in ecef_cov],
            "receivers": [r.uid for r in receivers], "distinct": distinct, "dof": dof, "error": error,
            "kalman_valid": bool(kalman.valid),
            "speed": None if kalman.ground_speed is None else fl(kalman.ground_speed),
            "heading": None if kalman.heading is None else fl(kalman.heading),
        }) + "\n")

    coord.add_output_handler(handler)

    receivers = {}
    users = set()
    next_15 = 0.0
    cpu0 = _real_process_time()

    for t, kind, p in events:
        # periodic work and timers that fall before this event
        while next_15 <= t:
            sched.run_until(start_epoch + next_15)
            _vnow[0] = start_epoch + next_15
            coord._write_state()
            coord.clock_tracker.clear_all_sync_points()
            next_15 += 15.0
        sched.run_until(start_epoch + t)
        _vnow[0] = start_epoch + t

        if kind == "hs":
            hs = p[1]
            user = re.sub("[^A-Za-z0-9_.-]", "_", str(hs["user"]))[:40]
            if len(user) < 3:
                user += "_00"
            base, n = user, 1
            while user in users:
                user = f"{base}_{n}"
                n += 1
            users.add(user)
            lon = float(hs["lon"])
            if lon > 180:
                lon -= 180
            receivers[p[0]] = coord.new_receiver(
                connection=StubConnection(), uuid=hs.get("uuid"), user=user, auth=None,
                position_llh=(float(hs["lat"]), lon, float(hs["alt"])),
                clock_type=str(hs.get("clock_type", "dump1090")),
                privacy=bool(hs.get("privacy", False)), connection_info="ref")
            continue

        r = receivers.get(p[0])
        if r is None or r.dead:
            continue
        if kind == "sync":
            coord.receiver_sync(r, p[1], p[2], p[3], p[4])
        elif kind == "mlat":
            now = _vnow[0]
            if r.bad_syncs > 0 or now - r.last_sync > 20:
                continue
            coord.receiver_mlat(r, p[1], p[2], now)
        elif kind == "seen":
            coord.receiver_tracking_add(r, p[1])
        elif kind == "lost":
            coord.receiver_tracking_remove(r, p[1])
        elif kind == "rate_report":
            coord.receiver_rate_report(r, p[1])
        elif kind == "input_connected":
            r.clock_reset("input connected")
            r.clock_reset_counter = 0
        elif kind == "clock_reset":
            r.clock_reset("clientmsg")
        elif kind == "close":
            users.discard(r.user)
            coord.receiver_disconnect(r)
        # calls scheduled with call_soon run before the next event
        sched.run_until(_vnow[0])

    cpu = _real_process_time() - cpu0
    out.close()
    print(json.dumps({"events": len(events), "results": n_results[0], "receivers": len(receivers),
                      "cpu_seconds": round(cpu, 3)}))


if __name__ == "__main__":
    main()
