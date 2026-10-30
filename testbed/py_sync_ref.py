#!/usr/bin/env python3
"""Deterministic reference for the clock-sync stage of the Python mlat-server.

Runs the *real* Coordinator / Tracker / ClockTracker (Cython) from this
repository on the sync-related events of a capture, with a virtual clock and no
sockets, so the result is repeatable. Used as the golden output for the Rust
port and as its CPU baseline. Run inside the baseline image:

    docker run --rm --entrypoint python3 \
        -v ~/Downloads:/data -v $PWD/testbed:/h mlat-baseline:local \
        /h/py_sync_ref.py /data/capture.jsonl /data/sync-golden.jsonl [--max-secs N]

Scope: connection setup, seen/lost tracking, sync messages, clock resets,
disconnects, the 1 s / 10 s pairing cleanup and the 15 s sync-point clear.
Not in scope (yet): mlat resolution, interest selection, and the 15 s health
evaluation (bad_syncs / sync_dont_use), so those stay at zero here.
"""
import json
import re
import sys
import tempfile
import time

sys.path.insert(0, "/opt/mlat-server")

# ---- virtual clock: the Cython code calls time.time() ----
_real_process_time = time.process_time
_vnow = [0.0]
time.time = lambda: _vnow[0]

from mlat import coordinator as coordinator_mod  # noqa: E402


class _Handle:
    def cancel(self):
        pass


class StubLoop:
    def call_later(self, *a, **k):
        return _Handle()

    def call_soon(self, *a, **k):
        return _Handle()

    def add_signal_handler(self, *a, **k):
        pass

    def remove_signal_handler(self, *a, **k):
        pass


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


FULL = '--full' in sys.argv
import os
PROFILE_ON = os.environ.get('MLAT_CPU_PROFILE', '0') != '0'


def load_events(path, max_secs):
    """-> (start_epoch, list of (t, kind, payload)) in capture order."""
    events = []
    start_epoch = None
    with open(path) as f:
        for line in f:
            if line.startswith('{"ev":"c2s"'):
                e = json.loads(line)
                if max_secs is not None and e["t"] > max_secs:
                    break
                l = e["l"]
                m = json.loads(l) if l[:14].lstrip("{ ").startswith(
                    ('"sync"', '"seen"', '"lost"', '"input_', '"clock_', '"rate_')) else None
                if m is None:
                    continue
                if "sync" in m:
                    s = m["sync"]
                    events.append((e["t"], "sync", (e["c"], float(s["et"]), float(s["ot"]),
                                                    bytes.fromhex(s["em"]), bytes.fromhex(s["om"]))))
                elif "seen" in m:
                    events.append((e["t"], "seen", (e["c"], {int(x, 16) for x in m["seen"]})))
                elif "lost" in m:
                    events.append((e["t"], "lost", (e["c"], {int(x, 16) for x in m["lost"]})))
                elif "rate_report" in m and FULL:
                    events.append((e["t"], "rate_report", (e["c"], {int(k, 16): v for k, v in m["rate_report"].items()})))
                elif "input_connected" in m:
                    events.append((e["t"], "input_connected", (e["c"],)))
                elif "input_disconnected" in m or "clock_reset" in m or "clock_jump" in m:
                    events.append((e["t"], "clock_reset", (e["c"],)))
            elif line.startswith('{"ev":"hs"'):
                e = json.loads(line)
                if max_secs is not None and e["t"] > max_secs:
                    continue
                events.append((e["t"], "hs", (e["c"], json.loads(e["line"]))))
            elif line.startswith('{"ev":"close"'):
                e = json.loads(line)
                if max_secs is not None and e["t"] > max_secs:
                    continue
                events.append((e["t"], "close", (e["c"],)))
            elif line.startswith('{"ev":"meta"'):
                start_epoch = json.loads(line)["start_epoch"]
    return start_epoch, events


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if "--snap-every" in sys.argv:
        args = [a for a in args if a != sys.argv[sys.argv.index("--snap-every") + 1]]
    max_secs = None
    if "--max-secs" in sys.argv:
        max_secs = float(sys.argv[sys.argv.index("--max-secs") + 1])
        args = [a for a in args if a != str(sys.argv[sys.argv.index("--max-secs") + 1])]
    capture, out_path = args[0], args[1]

    start_epoch, events = load_events(capture, max_secs)
    print(f"loaded {len(events)} events", file=sys.stderr)

    workdir = tempfile.mkdtemp()
    _vnow[0] = start_epoch
    coord = coordinator_mod.Coordinator(work_dir=workdir, loop=StubLoop(), status_interval=-1, partition=(1, 1))
    tracker, ctrack = coord.tracker, coord.clock_tracker

    receivers = {}   # capture connection id -> Receiver
    users = set()

    def snapshot(t):
        pairs = []
        for (r0, r1), p in ctrack.clock_pairs.items():
            pairs.append([r0.uid, r1.uid, p.n, bool(p.valid), p.variance, p.factor, p.i_factor,
                          p.base_avg, p.peer_avg, round(p.updated - start_epoch, 6)])
        pairs.sort()
        return {"t": t, "pairs": pairs}

    snaps = []
    stat = {'untracked': 0, 'tracked': 0, 'dead': 0}
    kind_cpu = {}
    kind_n = {}
    clears = []
    snap_every = float(sys.argv[sys.argv.index('--snap-every') + 1]) if '--snap-every' in sys.argv else 60.0
    next_snap = snap_every
    next_cleanup = 1.0
    next_clear = 0.0
    sync_events = 0
    cpu = 0.0
    snap_cpu = 0.0
    t_begin = _real_process_time()
    last_t = 0.0

    for t, kind, p in events:
        # timers that would have fired before this event
        while True:
            due = min(next_cleanup, next_clear)
            if due > t:
                break
            _vnow[0] = start_epoch + due
            if next_clear <= next_cleanup:
                sp = ctrack.sync_points
                usable = sum(1 for v in sp.values() if v[1] != -1)
                multi = sum(1 for v in sp.values() if v[1] != -1 and len(v[1]) > 0)
                clears.append((round(due, 1), len(sp), usable, multi))
                ctrack.clear_all_sync_points()
                next_clear += 15.0
            else:
                ctrack._cleanup()
                next_cleanup += 10.0
        while next_snap <= t:
            s0 = _real_process_time()
            snaps.append(snapshot(next_snap))
            snap_cpu += _real_process_time() - s0
            next_snap += snap_every

        _vnow[0] = start_epoch + t
        last_t = t
        _k0 = _real_process_time()

        if kind == "sync":
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                if tracker.aircraft.get((p[3][1] << 16) | (p[3][2] << 8) | p[3][3]) is None:
                    stat['untracked'] += 1
                else:
                    stat['tracked'] += 1
                coord.receiver_sync(r, p[1], p[2], p[3], p[4])
                sync_events += 1
            else:
                stat['dead'] += 1
        elif kind == "hs":
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
        elif kind == "close":
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                users.discard(r.user)
                coord.receiver_disconnect(r)
        elif kind in ("seen", "lost"):
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                # bypass interest selection (out of scope): update tracking sets only
                if FULL:
                    (coord.receiver_tracking_add if kind == "seen" else coord.receiver_tracking_remove)(r, p[1])
                else:
                    (tracker.add if kind == "seen" else tracker.remove)(r, p[1])
        elif kind == "rate_report":
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                coord.receiver_rate_report(r, p[1])
        elif kind == "input_connected":
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                r.clock_reset("input connected")
                r.clock_reset_counter = 0
        elif kind == "clock_reset":
            r = receivers.get(p[0])
            if r is not None and not r.dead:
                r.clock_reset("clientmsg")
        kind_cpu[kind] = kind_cpu.get(kind, 0.0) + (_real_process_time() - _k0)
        kind_n[kind] = kind_n.get(kind, 0) + 1

    cpu = _real_process_time() - t_begin - snap_cpu
    snaps.append(snapshot(round(last_t, 3)))

    with open(out_path, "w") as f:
        for s in snaps:
            f.write(json.dumps(s) + "\n")
    summary = {"events": len(events), "sync_events": sync_events, "receivers": len(receivers),
               "pairs_final": len(snaps[-1]["pairs"]), "cpu_seconds": round(cpu, 3),
               "us_per_sync_event": round(cpu / max(1, sync_events) * 1e6, 2)}
    summary['event_aircraft_state'] = stat
    if PROFILE_ON:
        from mlat import profile
        import io
        buf = io.StringIO(); profile.dump_cpu_profiles(buf); print(buf.getvalue(), file=sys.stderr)
    summary['cpu_by_kind_s'] = {k: round(v, 3) for k, v in kind_cpu.items()}
    summary['count_by_kind'] = kind_n
    summary['sync_table_per_15s'] = clears[10:14]
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
