#!/usr/bin/env python3
"""Record a Basestation (SBS) output stream: sbs_tap.py PORT OUTFILE.
Retries the connection for a few seconds; flushes every line."""
import socket, sys, time

port, path = int(sys.argv[1]), sys.argv[2]
for _ in range(50):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=5)
        break
    except OSError:
        time.sleep(0.2)
else:
    sys.exit("sbs_tap: could not connect to port %d" % port)
s.settimeout(None)
with open(path, "wb", buffering=0) as out:
    while True:
        data = s.recv(65536)
        if not data:
            break
        out.write(data)
