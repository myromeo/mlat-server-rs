#!/usr/bin/env python3
"""Generate golden values for the Rust port from the real compiled Python modules.

    docker run --rm --entrypoint python3 -v $PWD:/out -v $PWD/testbed:/h mlat-baseline:local \
        /h/gen_modes_golden.py /out/crates/mlat-core/tests/fixtures/modes_golden.json
"""
import json, random, sys
sys.path.insert(0, "/opt/mlat-server")
import modes_cython.message as M
from mlat import geodesy

random.seed(20261030)


def crc_fix(b):
    """Make a 14-byte message have a zero CRC residual."""
    b = bytearray(b)
    b[11] = b[12] = b[13] = 0
    r = M.crc_residual(bytes(b))
    b[11], b[12], b[13] = (r >> 16) & 0xff, (r >> 8) & 0xff, r & 0xff
    assert M.crc_residual(bytes(b)) == 0
    return bytes(b)


messages = []
for i in range(6000):
    b = bytearray(random.getrandbits(8) for _ in range(14))
    r = random.random()
    if r < 0.75:
        b[0] = (17 << 3) | (b[0] & 7)           # DF17
    elif r < 0.85:
        b[0] = (18 << 3) | (b[0] & 7)           # DF18
    if random.random() < 0.7:
        b = bytearray(crc_fix(b))
    m = M.decode(bytes(b))
    rec = {"hex": bytes(b).hex(), "df": (b[0] >> 3) & 0x1f}
    if m is not None and getattr(m, "DF", None) == 17:
        rec["decoded"] = {
            "address": m.address, "crc_ok": bool(m.crc_ok),
            "estype": m.estype.name, "nuc": m.nuc,
            "altitude": m.altitude,
        }
        if m.estype.name == "airborne_position":
            rec["decoded"].update({"F": m.F, "LAT": m.LAT, "LON": m.LON})
    messages.append(rec)

# exhaustive AC12 -> altitude, via a crafted DF17 airborne position
ac12 = []
for v in range(4096):
    b = bytearray(14)
    b[0] = 17 << 3
    b[4] = 11 << 3
    b[5] = v >> 4
    b[6] = (v & 0xf) << 4
    m = M.decode(bytes(b))
    ac12.append(m.altitude)

cpr = []
for i in range(5000):
    if i < 4000:
        a = [random.randrange(131072) for _ in range(4)]
    else:   # same-zone pairs, the common case
        lat = random.uniform(-85, 85)
        a = [random.randrange(131072) for _ in range(4)]
    try:
        out = list(M.decode_cpr(*[float(x) for x in a]))
    except ValueError:
        out = None
    cpr.append({"in": a, "out": out})

llh = []
for i in range(2000):
    p = [random.uniform(-90, 90), random.uniform(-180, 180), random.uniform(-1000, 14000)]
    q = [random.uniform(-90, 90), random.uniform(-180, 180), random.uniform(-1000, 14000)]
    e1, e2 = geodesy.llh2ecef(p), geodesy.llh2ecef(q)
    llh.append({"llh": p, "ecef": list(e1), "other_llh": q, "dist": geodesy.ecef_distance(e1, e2)})

json.dump({"messages": messages, "ac12": ac12, "cpr": cpr, "llh": llh}, open(sys.argv[1], "w"))
print("wrote", sys.argv[1], len(messages), len(cpr), len(llh))
