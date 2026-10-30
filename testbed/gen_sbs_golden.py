#!/usr/bin/env python3
"""Golden cases for the Basestation (SBS) line formatter, produced by calling the
real BasestationClient.write_result with random inputs.

    docker run --rm --entrypoint python3 -v $PWD:/out -v $PWD/testbed:/h mlat-baseline:local \
        /h/gen_sbs_golden.py /out/crates/mlat-core/tests/fixtures/sbs_golden.json
"""
import json, math, random, sys, time as _time
sys.path.insert(0, "/opt/mlat-server")
from mlat import output, geodesy

random.seed(77)


class W:
    def __init__(self):
        self.lines = []

    def write(self, b):
        self.lines.append(b.decode("ascii"))


class K:
    pass


class AC:
    pass


class Coord:
    pass


NOW = [0.0]
output.time = type("T", (), {"time": staticmethod(lambda: NOW[0]), "strftime": staticmethod(_time.strftime),
                             "gmtime": staticmethod(_time.gmtime)})()


def fl(x):
    return x if (x is None or math.isfinite(x)) else str(x)


cases = []
callsigns = [None, "", "AOS47   ", "BAW123", 'A"B', "A,B", "TEST\nX", "G-ABCD"]
squawks = [None, "1253", "7700", "0000", "4,3"]
for i in range(6000):
    use_kalman = random.random() < 0.7
    ac = AC()
    ac.callsign = random.choice(callsigns)
    ac.squawk = random.choice(squawks)
    recv_t = 1.79e9 + random.uniform(0, 8e5) + (random.choice([0.0, 0.9996, 0.9994, 0.0004, 0.5]) if random.random() < 0.3 else random.random())
    ac.last_altitude_time = random.choice([None, 0, recv_t - 1, recv_t - 4.9, recv_t - 5.1, recv_t - 100])
    ac.altitude = random.choice([0, 4000, 35000, -400]) if ac.last_altitude_time is not None else None
    ac.vrate_time = random.choice([None, recv_t - 1, recv_t - 6])
    ac.vrate = random.choice([0, 1500, -2000]) if ac.vrate_time is not None else None
    ac.last_crappy_output = random.choice([0, recv_t - 10, recv_t - 40, recv_t - 100, recv_t])
    pre_crappy = ac.last_crappy_output
    lat, lon, alt = random.uniform(-60, 70), random.uniform(-179, 179), random.uniform(-100, 14000)
    ecef = list(geodesy.llh2ecef([lat, lon, alt]))
    k = K()
    k.valid = random.random() < 0.6
    k.last_update = recv_t + random.choice([-1, 0, 1])
    k.ground_speed = random.uniform(0, 300)
    k.heading = random.uniform(0, 360) if random.random() < 0.9 else random.choice([0.5, 1.5, 2.5, 359.5])
    k.vertical_speed = random.uniform(-20, 20)
    dof = random.choice([0, 0, 1, 2, 3])
    receivers = [object()] * random.randint(3, 9)
    error = random.choice([None, 150, 999, 4000])
    NOW[0] = recv_t + random.uniform(0.9, 1.5)

    client = object.__new__(output.BasestationClient)
    client.coordinator = Coord()
    client.coordinator.tracker = type("Tr", (), {"aircraft": {0x4ca6aa: ac}})()
    client.writer = W()
    client.use_kalman_data = use_kalman
    client.last_output = 0
    client.logger = type("L", (), {"exception": staticmethod(lambda *a, **k: None)})()
    client.write_result(recv_t, 0x4ca6aa, ecef, None, receivers, 4, dof, k, error)
    line = client.writer.lines[0] if client.writer.lines else None
    cases.append({
        "use_kalman": use_kalman, "recv_t": recv_t, "now": NOW[0], "ecef": ecef,
        "callsign": ac.callsign, "squawk": ac.squawk, "altitude": ac.altitude,
        "last_altitude_time": ac.last_altitude_time, "vrate": ac.vrate, "vrate_time": ac.vrate_time,
        "last_crappy_in": pre_crappy, "last_crappy_out": ac.last_crappy_output,
        "kalman": {"valid": k.valid, "last_update": k.last_update, "ground_speed": k.ground_speed,
                   "heading": k.heading, "vertical_speed": k.vertical_speed},
        "dof": dof, "n_receivers": len(receivers), "error": error, "line": line,
    })
json.dump(cases, open(sys.argv[1], "w"))
print("wrote", len(cases), "cases;", sum(1 for c in cases if c["line"] is None), "suppressed")
