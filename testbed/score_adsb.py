#!/usr/bin/env python3
"""Scores multilateration output against ADS-B ground truth.

    score_adsb.py capture.jsonl OUT1.sbs [OUT2.sbs ...] [--truth-sbs FILE]

Ground truth is the DF17 airborne-position messages the receivers themselves
reported in their `sync` messages, decoded with CPR relative to the reporting
receiver. Aircraft that are both on ADS-B and multilaterated (the server forces
mlat on some of them so it can be checked) are scored: each Basestation
position in OUT is compared with the truth track interpolated to the same
instant. Give several outputs (e.g. the Python and the Rust server's sbs.txt
from the same replay) to compare them under identical truth.

The truth is timestamped with the time the server received the message, so it
carries network jitter of a few tens of ms (about 10 m for a jet); read the
absolute numbers as an upper bound and the difference between servers as the
result. `--truth-sbs` takes an SBS file from an independent ADS-B decoder instead.
"""
import bisect
import calendar
import json
import math
import sys
import time
from collections import defaultdict

NZ = 15


def nl(lat):
    lat = abs(lat)
    if lat < 1e-9:
        return 59
    if abs(lat - 87) < 1e-9:
        return 2
    if lat > 87:
        return 1
    a = 1 - math.cos(math.pi / (2 * NZ))
    b = math.cos(math.pi * lat / 180) ** 2
    return int(math.floor(2 * math.pi / math.acos(1 - a / b)))


def cpr_local(ref_lat, ref_lon, odd, lat_cpr, lon_cpr):
    dlat = 360.0 / (4 * NZ - (1 if odd else 0))
    lat_f = lat_cpr / 131072.0
    j = math.floor(ref_lat / dlat) + math.floor(0.5 + (ref_lat % dlat) / dlat - lat_f)
    lat = dlat * (j + lat_f)
    ni = max(nl(lat) - (1 if odd else 0), 1)
    dlon = 360.0 / ni
    lon_f = lon_cpr / 131072.0
    m = math.floor(ref_lon / dlon) + math.floor(0.5 + (ref_lon % dlon) / dlon - lon_f)
    lon = dlon * (m + lon_f)
    if lon >= 180:
        lon -= 360
    return lat, lon


def decode_df17_position(hexmsg, ref_lat, ref_lon):
    try:
        b = bytes.fromhex(hexmsg)
    except ValueError:
        return None
    if len(b) != 14 or (b[0] >> 3) != 17:
        return None
    me = int.from_bytes(b[4:11], "big")
    tc = me >> 51
    if not (9 <= tc <= 18 or 20 <= tc <= 22):
        return None
    odd = (me >> 34) & 1
    lat_cpr = (me >> 17) & 0x1FFFF
    lon_cpr = me & 0x1FFFF
    icao = int.from_bytes(b[1:4], "big")
    lat, lon = cpr_local(ref_lat, ref_lon, odd, lat_cpr, lon_cpr)
    # a local decode is only trusted within 180 NM of the receiver
    if haversine(ref_lat, ref_lon, lat, lon) > 333000:
        return None
    return icao, lat, lon


def haversine(la1, lo1, la2, lo2):
    p1, p2 = math.radians(la1), math.radians(la2)
    dl = math.radians(lo2 - lo1)
    dp = p2 - p1
    a = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * 6371008.8 * math.asin(math.sqrt(a))


def truth_from_capture(path):
    start = None
    pos = {}
    tracks = defaultdict(list)
    with open(path) as f:
        for line in f:
            if line.startswith('{"ev":"meta"'):
                start = json.loads(line)["start_epoch"]
            elif line.startswith('{"ev":"hs"'):
                e = json.loads(line)
                hs = json.loads(e["line"])
                try:
                    pos[e["c"]] = (float(hs["lat"]), float(hs["lon"]))
                except (KeyError, ValueError):
                    pass
            elif line.startswith('{"ev":"c2s"') and "sync" in line[:100]:
                e = json.loads(line)
                if '"sync"' not in e["l"][:20] or e["c"] not in pos:
                    continue
                s = json.loads(e["l"])["sync"]
                for k in ("em", "om"):
                    r = decode_df17_position(s[k], *pos[e["c"]])
                    if r:
                        tracks[r[0]].append((e["t"], r[1], r[2]))
    for v in tracks.values():
        v.sort()
    return start, tracks


def truth_from_sbs(path, start):
    tracks = defaultdict(list)
    for row in read_sbs(path, start):
        tracks[row[0]].append(row[1:4])
    for v in tracks.values():
        v.sort()
    return tracks


def read_sbs(path, start):
    """(icao, t_since_start, lat, lon, receivers) for each MSG,3 line with a position."""
    with open(path, errors="replace") as f:
        for line in f:
            p = line.rstrip("\r\n").split(",")
            if len(p) < 19 or p[0] != "MSG" or not p[14] or not p[15]:
                continue
            try:
                ts = calendar.timegm(time.strptime(p[6] + " " + p[7][:8], "%Y/%m/%d %H:%M:%S")) + float("0" + p[7][8:])
                yield int(p[4], 16), ts - start, float(p[14]), float(p[15]), int(p[18] or 0)
            except ValueError:
                continue


def interpolate(track, t, max_gap=6.0):
    i = bisect.bisect_left(track, (t,))
    if i == 0 or i == len(track):
        return None
    (t0, la0, lo0), (t1, la1, lo1) = track[i - 1], track[i]
    if t1 - t0 > max_gap or t1 == t0:
        return None
    f = (t - t0) / (t1 - t0)
    return la0 + f * (la1 - la0), lo0 + f * (lo1 - lo0)


def replay_shift(path, start):
    """The exact shift for a replayed output, from the replay-report.txt beside it."""
    import os
    import re
    rep = os.path.join(os.path.dirname(os.path.abspath(path)), "replay-report.txt")
    try:
        m = re.search(r"replay start epoch\s+([0-9.]+)", open(rep).read())
    except OSError:
        return None
    return float(m.group(1)) - start if m else None


def align(rows, tracks):
    """Seconds to subtract from an output's times so they fall on the capture's.

    Zero for a run made while the capture was recorded. A replay happens at a
    different wall-clock time, so the shift is found by looking for the one
    that puts the first positions on top of the truth tracks.
    """
    if not rows:
        return 0.0
    first = rows[0][1]
    if abs(first) < 600:
        return 0.0
    sample = [r for r in rows[:4000] if r[0] in tracks][:300]

    def hits(d, tol=4000.0):
        n = 0
        for icao, t, lat, lon, _ in sample:
            ref = interpolate(tracks[icao], t - d)
            if ref is not None and haversine(lat, lon, *ref) < tol:
                n += 1
        return n

    best = max((hits(first - 300 + 0.5 * i), first - 300 + 0.5 * i) for i in range(1200))
    fine = max((hits(best[1] - 0.5 + 0.02 * i, 1500.0), best[1] - 0.5 + 0.02 * i) for i in range(50))
    return fine[1]


def pct(v, p):
    return v[min(len(v) - 1, int(p / 100 * len(v)))]


def main():
    argv = sys.argv[1:]
    truth_sbs = None
    if "--truth-sbs" in argv:
        i = argv.index("--truth-sbs")
        truth_sbs = argv[i + 1]
        del argv[i:i + 2]
    capture, outs = argv[0], argv[1:]
    start, tracks = truth_from_capture(capture)
    if truth_sbs:
        tracks = truth_from_sbs(truth_sbs, start)
    print(f"truth: {sum(len(v) for v in tracks.values())} positions on {len(tracks)} aircraft")

    # load every output and put it on the capture's clock
    loaded = []
    for path in outs:
        data = list(read_sbs(path, start))
        shift = replay_shift(path, start)
        if shift is None:
            shift = align(data, tracks)
            if shift:
                print(f"{path}: replayed output, shift estimated at {shift:.2f} s (no replay-report.txt beside it)")
        else:
            print(f"{path}: replayed output, shifted by {shift:.2f} s (from its replay-report.txt)")
        loaded.append((path, [(i, t - shift, la, lo, n) for i, t, la, lo, n in data]))

    def score(data, lag):
        out = []
        for icao, t, lat, lon, n in data:
            tr = tracks.get(icao)
            ref = interpolate(tr, t + lag) if tr else None
            if ref is None:
                continue
            d = haversine(lat, lon, *ref)
            if d <= 20000:  # further than that is another aircraft, not an accuracy sample
                out.append((d, n))
        return out

    # The truth carries the delay with which receivers deliver sync messages.
    # It is the same for every output, so one global lag is fitted and applied.
    lag = 0.0
    if truth_sbs is None:
        def med(l):
            allv = sorted(d for _, data in loaded for d, _ in score(data, l))
            return pct(allv, 50) if allv else float("inf")
        lag = min((med(-4.0 + 0.1 * i), -4.0 + 0.1 * i) for i in range(81))[1]
        print(f"truth delay fitted: {lag:+.1f} s (common to all outputs)")

    rows = []
    for path, data in loaded:
        errs = defaultdict(list)
        for d, n in score(data, lag):
            errs["all"].append(d)
            errs[f"{min(n, 6)}{'+' if n >= 6 else ''} rx"].append(d)
        rows.append((path, len(data), errs))

    for path, total, errs in rows:
        print(f"\n{path}: {total} positions, {len(errs['all'])} scored against ADS-B truth")
        print(f"  {'group':8} {'n':>6} {'median':>8} {'p90':>8} {'p95':>8} {'p99':>8} {'max':>9}   (metres)")
        for k in sorted(errs):
            v = sorted(errs[k])
            if not v:
                continue
            print(f"  {k:8} {len(v):6d} {pct(v, 50):8.1f} {pct(v, 90):8.1f} {pct(v, 95):8.1f} {pct(v, 99):8.1f} {v[-1]:9.1f}")


if __name__ == "__main__":
    main()
