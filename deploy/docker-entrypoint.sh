#!/bin/sh
# Builds the mlat-server-rs command line from the MLAT_* environment variables
# (see example.env). Arguments given to the container are appended as they are.
set -eu

# Run as the unprivileged 'mlat' user, but first make the state directory writable by it
# (a volume left behind by another server may be owned by root).
if [ "$(id -u)" = 0 ]; then
    chown -R mlat:mlat "${MLAT_WORK_DIR_IN_CONTAINER:-/run/mlat-server}" 2>/dev/null || true
    exec setpriv --reuid=mlat --regid=mlat --init-groups "$0" "$@"
fi

args="--work-dir ${MLAT_WORK_DIR_IN_CONTAINER:-/run/mlat-server}"

add_each() { # add_each FLAG "value value ..."
    for v in $2; do args="$args $1 $v"; done
}

add_each --client-listen "${MLAT_CLIENT_LISTEN:-31090}"
add_each --filtered-basestation-listen "${MLAT_FILTERED_LISTEN:-}"
add_each --basestation-listen "${MLAT_UNFILTERED_LISTEN:-}"
add_each --filtered-basestation-connect "${MLAT_FILTERED_CONNECT:-}"
add_each --basestation-connect "${MLAT_UNFILTERED_CONNECT:-}"
add_each --write-csv "${MLAT_WRITE_CSV:-}"

[ -n "${MLAT_PARTITION:-}" ] && args="$args --partition $MLAT_PARTITION"
[ -n "${MLAT_STATUS_INTERVAL:-}" ] && args="$args --status-interval $MLAT_STATUS_INTERVAL"
[ -n "${MLAT_ENGINE_THREADS:-}" ] && args="$args --engine-threads $MLAT_ENGINE_THREADS"
[ -n "${MLAT_IO_THREADS:-}" ] && args="$args --io-threads $MLAT_IO_THREADS"
[ -n "${MLAT_SOURCE_URL:-}" ] && args="$args --source-url $MLAT_SOURCE_URL"
[ -n "${MLAT_DUMP_PSEUDORANGE:-}" ] && args="$args --dump-pseudorange $MLAT_DUMP_PSEUDORANGE"

# the MOTD may contain spaces, so it is passed as a single argument
if [ -n "${MLAT_MOTD:-}" ]; then
    # shellcheck disable=SC2086
    exec /usr/local/bin/mlat-server-rs $args --motd "$MLAT_MOTD" "$@"
fi
# shellcheck disable=SC2086
exec /usr/local/bin/mlat-server-rs $args "$@"
