#!/usr/bin/env python3
"""Check that two work directories hold state files of the same shape:
same fields, in the same order, with the same JSON types (int vs float matters).

    compare_state_files.py PYTHON_DIR RUST_DIR
"""
import json, sys, collections


def tname(v):
    return {bool: "bool", int: "int", float: "float", str: "str", type(None): "null", list: "list", dict: "dict"}[type(v)]


def shape(entries):
    """field -> set of types, plus the order fields appear in."""
    types = collections.defaultdict(set)
    orders = []
    for e in entries:
        orders.append(list(e.keys()))
        for k, v in e.items():
            types[k].add(tname(v))
    return types, orders


def merged_order(orders):
    """The overall field order, from the longest example plus later additions."""
    out = []
    for o in sorted(orders, key=len, reverse=True):
        for k in o:
            if k not in out:
                out.append(k)
    return out


def check(name, py_entries, rs_entries, ok_missing=()):
    ptypes, porders = shape(py_entries)
    rtypes, rorders = shape(rs_entries)
    problems = []
    porder = merged_order(porders)
    for k, ts in rtypes.items():
        if k not in ptypes:
            problems.append(f"field '{k}' only in rust")
        elif not ts <= ptypes[k]:
            problems.append(f"field '{k}': rust types {sorted(ts)} not among python's {sorted(ptypes[k])}")
    for k in ptypes:
        if k not in rtypes and k not in ok_missing:
            problems.append(f"field '{k}' (python types {sorted(ptypes[k])}) never present in rust")
    # relative order of the fields rust emits must agree with python's
    for o in rorders:
        idx = [porder.index(k) for k in o if k in porder]
        if idx != sorted(idx):
            problems.append(f"field order differs: {o[:8]}...")
            break
    print(f"{name}: python {len(py_entries)} entries, rust {len(rs_entries)} entries -> {'SAME SHAPE' if not problems else 'DIFFERENCES'}")
    for p in problems:
        print("   ", p)
    return not problems


def main():
    pd, rd = sys.argv[1].rstrip("/") + "/", sys.argv[2].rstrip("/") + "/"
    ok = True
    # clients.json: one dict per user
    p, r = json.load(open(pd + "clients.json")), json.load(open(rd + "clients.json"))
    ok &= check("clients.json", list(p.values()), list(r.values()))
    # sync.json: per user {peers, bad_syncs, lat, lon}; peers: user -> 8-item list
    p, r = json.load(open(pd + "sync.json")), json.load(open(rd + "sync.json"))
    ok &= check("sync.json (receivers)", list(p.values()), list(r.values()))
    pp = [v for e in p.values() for v in e["peers"].values()]
    rp = [v for e in r.values() for v in e["peers"].values()]
    pt = collections.defaultdict(set)
    rt = collections.defaultdict(set)
    for v in pp:
        for i, x in enumerate(v):
            pt[i].add(tname(x))
    for v in rp:
        for i, x in enumerate(v):
            rt[i].add(tname(x))
    bad = [(i, sorted(rt[i]), sorted(pt[i])) for i in rt if not rt[i] <= pt[i]]
    print(f"sync.json (peer arrays): python {len(pp)} rust {len(rp)} lengths {sorted({len(v) for v in pp})}/{sorted({len(v) for v in rp})} -> {'SAME SHAPE' if not bad else 'DIFFERENCES ' + str(bad)}")
    ok &= not bad
    # aircraft.json
    p, r = json.load(open(pd + "aircraft.json")), json.load(open(rd + "aircraft.json"))
    ok &= check("aircraft.json", list(p.values()), list(r.values()))
    sys.exit(0 if ok else 1)


main()
