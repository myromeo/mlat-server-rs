# mlat-server-rs

A multi-threaded Rust rewrite of the [mlat-server](https://github.com/wiedehopf/mlat-server)
multilateration server (itself a fork of Oliver Jowett's mlat-server). It speaks the same wire
protocol, so receivers need no change, and it writes the same Basestation output and state files,
so readsb and the web front ends need no change either. One process uses every core it is given.

Licence: AGPL-3.0-or-later, as the original. Anyone who connects to a running server must be able
to obtain its source: set `--source-url` / `MLAT_SOURCE_URL` to where this repository is published.

## Quick start

    cp example.env .env && docker compose build && docker compose up -d

Receivers connect to port 31090 as before, the only port published to the host. readsb reads the
Kalman-filtered Basestation output on 31003 over the docker network
(`--net-connector=mlat-server,31003,sbs_in`). The server gets 1 CPU by default; raise `MLAT_CPUS`
in `.env` as stations are added. `sync.json`, `clients.json` and `aircraft.json` appear in the
`mlat-work` volume. Without Docker:

    cargo build --release -p mlat-server --features glibc-pow
    target/release/mlat-server-rs --client-listen 31090 --filtered-basestation-listen 31003 --work-dir DIR

* Deploying: [docs/DEPLOY.md](docs/DEPLOY.md) (`docker compose`, every setting in `example.env`)
* Replacing a running Python server: [docs/CUTOVER.md](docs/CUTOVER.md)

## Measured against the Python server

Both servers were fed the same 14-minute recording of live traffic from 139 receivers (replayed in
real time as separate connections, so the load is real) in identical Docker containers:

| | Python | mlat-server-rs |
|---|---|---|
| CPU used, whole replay | 148 s | 60 s (2.5x less) |
| Average load | 17.6% of one core | 7.1% of one core |
| Position calculation alone | 13.9 s | 2.6 s (5x less) |

The saving that matters is that Rust spreads over every core it is given, where Python is held to
one. Positions agree with the Python server (93% of positions have a counterpart in the other
server's output, median difference 12 m; two runs of the Python server against itself differ by
5-6 m) and score the same against ADS-B truth (median error 105 m against 107 m for Python,
measured on aircraft that were both multilaterated and broadcasting ADS-B; the truth itself carries
network jitter). Figures are from an arm64 Docker VM, not a production host.

## Command line

Every option of the Python server is accepted:

| option | |
|---|---|
| `--client-listen [host:]tcp[:udp]` | receiver connections; the UDP port enables the UDP transport |
| `--filtered-basestation-listen [host:]port` | Kalman-filtered Basestation output (what readsb reads) |
| `--basestation-listen [host:]port` | unfiltered Basestation output |
| `--filtered-basestation-connect host:port`, `--basestation-connect host:port` | connect out and send results, reconnecting every 30 s |
| `--write-csv FILE` | results as CSV, rotating at 1 MB, ten backups |
| `--dump-pseudorange FILE` | solver inputs as JSON lines; SIGHUP reopens the file |
| `--partition I/N` | share the aircraft between N servers (same hash as the original) |
| `--work-dir DIR` | state files: `sync.json`, `clients.json`, `aircraft.json`, `handshakes.log` |
| `--no-aircraft-json` | don't write `aircraft.json` (about 1 MB every 15 s); a saving where nothing reads it |
| `--motd`, `--status-interval`, `--tag`, `--check-leaks` | as the original (`--check-leaks` does nothing) |

Added: `--no-aircraft-json`, `--engine-threads N` (position engine threads, default 1), `--io-threads N` (connection
threads, default min(cores, 4)) and `--source-url URL` (the AGPL source location sent to receivers).

## How it works

    receivers --TCP/UDP--> connection threads (tokio) --> position engine --> results
                                                              |                  |
                          interest worker <- seen/lost/rate   +- pairer threads   +- per-receiver results
                          (what each receiver is asked        |  (clock sync)     +- Basestation clients
                           to send)                           +- matcher threads  +- CSV / pseudorange files
                                                                 (tracking, mlat,
                                                                  solver, Kalman)

* **Connection threads** read, decompress (`zlib2`, `zlib`, `none`) and parse each receiver's
  messages; this is most of the CPU at scale and spreads across all cores.
* **Matcher threads** own the aircraft, sharded by ICAO address: message matching, interest
  flags, grouping of mlat copies, the least-squares solve and the Kalman filter.
* **Pairer threads** own receiver-pair clock synchronisation, sharded by receiver pair. Matchers
  publish sync points with a global sequence number and the pairers apply them in that order, so
  the clock models do not depend on thread timing.
* The **clock thread** drives group deadlines, pairing cleanup, the 15 s health evaluation that
  writes the state files, and heartbeats.

The numerical core is a port of the Python server's: MINPACK `lmdif` for the least-squares solve,
the unscented Kalman filter, the clock-pair estimation and the group collection, including the
Python server's quirks, so results agree with the original given the same input.

## Differences from the Python server

* The order of peers in `sync.json` / `bad_peer_list` and of the interest lists follows receiver
  id, not hash or insertion order.
* `aircraft.json` `elapsed_seen` is larger for aircraft that only appear in rate reports.
* The `quine` debugging message is ignored.
* Datagrams on the UDP transport are authenticated only by the key the receiver was given and, as
  in the original, bypass the "no mlat from a receiver with bad sync" gate that the TCP path has.
