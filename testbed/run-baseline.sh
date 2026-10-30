#!/usr/bin/env bash
# Replay a capture into a throwaway copy of the Python mlat-server and keep
# everything it produced.
#   run-baseline.sh LABEL HOSTPORT CAPTURE.jsonl OUTDIR [replay args...]
# HOSTPORT is the loopback port for the client listener; HOSTPORT+1000 is used
# for the SBS output that readsb would normally read.
set -euo pipefail
label=$1; port=$2; capture=$3; outdir=$4/$label; shift 4
here=$(cd "$(dirname "$0")" && pwd)
tool="$here/../target/release/mlat-capture"
name=mlat-base-$label
sbsport=$((port + 1000))
mkdir -p "$outdir"

docker rm -f "$name" >/dev/null 2>&1 || true
# GC_OFF=1 runs the same server with the cyclic garbage collector disabled
extra=(); [ "${GC_OFF:-0}" = 1 ] && extra=(--entrypoint python3 -v "$here/gc_wrap.py:/opt/gc_wrap.py:ro")
img=(mlat-baseline:local); [ "${GC_OFF:-0}" = 1 ] && img=(mlat-baseline:local /opt/gc_wrap.py)
# RUST_SERVER=1 runs the Rust server (testbed/rust-bin/mlat-server-rs) instead of the Python one
if [ "${RUST_SERVER:-0}" = 1 ]; then
    extra=(--entrypoint /mlat-server-rs -v "$here/rust-bin/mlat-server-rs:/mlat-server-rs:ro")
    img=(debian:bookworm-slim)
fi
# RECORD=1 logs the numerical core's inputs/outputs to $outdir/rec (golden data for the Rust port)
if [ "${RECORD:-0}" = 1 ]; then
    mkdir -p "$outdir/rec"
    extra=(--entrypoint python3 -v "$here/record_wrap.py:/opt/record_wrap.py:ro" -v "$outdir/rec:/rec")
    img=(mlat-baseline:local /opt/record_wrap.py)
fi
docker run -d --name "$name" -e MLAT_CPU_PROFILE=${PROFILE:-0} --tmpfs /run/mlat-server ${extra[@]+"${extra[@]}"} \
    -p 127.0.0.1:$port:31090 -p 127.0.0.1:$sbsport:31003 \
    "${img[@]}" \
    --client-listen=31090 --filtered-basestation-listen=31003 \
    --work-dir=/run/mlat-server --status-interval=120 >/dev/null

# Docker's port forwarder accepts connections before the server is up, so wait
# for the server's own log lines rather than probing the port.
for _ in $(seq 1 150); do
    logs=$(docker logs "$name" 2>&1)
    [[ "$logs" == *"Basestation output listener listening"* && "$logs" == *"JSON client handler listening"* ]] && break
    sleep 0.2
done
python3 -I "$here/sbs_tap.py" "$sbsport" "$outdir/sbs.txt" &
nc_pid=$!

# SNAPSHOTS=1: copy the server's state files every minute while it runs
if [ "${SNAPSHOTS:-0}" = 1 ]; then
    (
        for i in $(seq 1 15); do
            sleep 60
            mkdir -p "$outdir/snap$i"
            for f in clients.json sync.json aircraft.json; do
                docker exec "$name" cat /run/mlat-server/$f > "$outdir/snap$i/$f" 2>/dev/null || true
            done
        done
    ) &
fi

# CPU seconds the server itself burns while the capture is replayed (cgroup v2)
cpu_usec() { docker exec "$name" sh -c "awk '/^usage_usec/{print \$2}' /sys/fs/cgroup/cpu.stat" 2>/dev/null || echo 0; }
cpu_before=$(cpu_usec); wall_before=$(date +%s)

"$tool" replay "$capture" --target 127.0.0.1:$port --results "$outdir/results.jsonl" "$@" \
    | tee "$outdir/replay-report.txt"

cpu_after=$(cpu_usec); wall_after=$(date +%s)
echo "server_cpu_seconds $(( (cpu_after - cpu_before) / 1000000 )) wall_seconds $(( wall_after - wall_before ))" | tee "$outdir/cpu.txt"

sleep 5
for f in clients.json aircraft.json sync.json cpuprofile.txt handshakes.log; do
    docker exec "$name" cat /run/mlat-server/$f > "$outdir/$f" 2>/dev/null || true
done
docker logs "$name" > "$outdir/server.log" 2>&1 || true
kill "$nc_pid" 2>/dev/null || true
docker rm -f "$name" >/dev/null
echo "done: $outdir"
