# mlat-server-rs

A multi-threaded Rust implementation of the [mlat-server](https://github.com/wiedehopf/mlat-server) multilateration server (wiedehopf's fork of Oliver Jowett's mlat-server). It is meant to be a drop-in replacement: same command-line options, same receiver protocol, same Basestation output, same state files. Receivers, readsb and anything that reads `sync.json` / `clients.json` need no change.

It is an independent project, not affiliated with or endorsed by the original authors. Licence: AGPL-3.0-or-later, as the original. Running since 30 October 2026 on [mlat.uk](https://mlat.uk) with about 140 receivers.

## Results

All figures below were measured. Where something was not measured, this README says so.

### CPU: the same traffic, before and after

mlat.uk, 18:20-18:30 UK time on the day before the switch and on the day of it, same host and same readsb:

| | Python mlat-server | mlat-server-rs |
|---|---|---|
| mlat-server CPU | 49.1% of one core | **21.4%** of one core |
| MLAT receivers / of which synced | 126 / 83 | 135 / 88 |
| Messages/s into readsb | 24,424 | 24,445 |

That is **about 2.2x less CPU on one core, with 7% more receivers** (0.39% of a core per receiver before, 0.16% after). The Python figure is its busiest thread, which is effectively the whole process (cross-checked against whole-process CPU). The mlat-server-rs window began a few minutes after a restart. A separate 10-minute sample from the container's cgroup counters, taken later in steady state, gave 22% (range 19-25%) with no CPU throttling. Resident memory is about 55 MiB.

A replay of 14 minutes of recorded production traffic (139 receivers connecting for real, identical containers) gave a similar ratio:

| | Python | mlat-server-rs |
|---|---|---|
| CPU used, whole replay | 148 s | 60 s (**2.5x** less) |
| Position calculation alone (no sockets) | 13.9 s | 2.6 s (5x less) |

Most of the server's CPU is network and protocol work (decompressing and parsing every receiver's messages), which is why the whole-server saving is smaller than the saving on the position maths.

### Multi-core: what is and is not shown

The server runs as several threads. At 143 receivers on mlat.uk its CPU time splits as:

| Thread(s) | Share of the process's CPU |
|---|---|
| connection handling (`io`; two threads on a 2-core limit) | 54% |
| matcher (message matching, grouping, solver, Kalman filter) | 31% |
| pairer (receiver-pair clock synchronisation) | 9% |
| clock (group deadlines, health pass) | 4% |
| interest, state-file writer, results | 2% |

The whole process is about 22% of one core, so the busiest single thread (the matcher) is at about 7% of a core, derived from those shares. Connections are served in parallel, and the position engine can be split across threads with `--engine-threads N`: aircraft are sharded by ICAO address and receiver-pair clock models by pair, with a global sequence number so that results do not depend on thread timing.

What is **not** established, and where feedback and load testing would be most useful:

* There is no measured "N cores carries M times the load" figure. Filling several cores needs several times today's receiver count, which has not been tested.
* More engine threads are not shown to help at today's size. Replaying the whole recording through the engine offline, as fast as one feeding thread can push it, took 1.6 s with one engine thread, 2.4 s with two and 3.3 s with four: at this scale the coordination costs more than it saves. `--engine-threads` therefore defaults to 1.
* Connection handling uses the cores the process is given (`--io-threads`, default min(cores, 4)). It is the part expected to matter first as receivers are added, but that is an expectation, not a measurement.

### Accuracy

Position agreement was checked on the same 14-minute recording, replayed into both servers:

* 93% of positions have a counterpart in the other server's output, with a **median difference of 12 m**. Two runs of the Python server against itself differ by 5-6 m, since the original's timing is not deterministic.
* Against ADS-B truth (aircraft that were both multilaterated and broadcasting ADS-B; a small sample, 68 and 92 positions) the median error was 107 m for Python and 105 m for mlat-server-rs. The truth carries network jitter of a few tens of ms, so the absolute figure is an upper bound.
* In deterministic mode, against a virtual-clock run of the original Python pipeline, the Rust pipeline reproduced all 1,830 results.

The numerical core is a port of the original's: MINPACK `lmdif` for the least-squares solve, the unscented Kalman filter, the clock-pair estimation and the group collection, including some quirks of the Python code (integer division in the drift filter, scipy's handling of zero-weight edges), so that results agree with the original given the same input.

## Drop-in compatibility

### Command line

Every option of `mlat-server` is accepted:

| option | notes |
|---|---|
| `--client-listen [host:]tcp[:udp]` | receivers; a UDP port enables the UDP transport |
| `--filtered-basestation-listen [host:]port` | Kalman-filtered Basestation output |
| `--basestation-listen [host:]port` | unfiltered Basestation output |
| `--filtered-basestation-connect host:port`, `--basestation-connect host:port` | connect out, reconnect every 30 s |
| `--write-csv FILE` | rotating, 1 MB x 10, same columns |
| `--dump-pseudorange FILE` | solver inputs as JSON lines; SIGHUP reopens the file |
| `--partition I/N` | same address hash as the original |
| `--work-dir DIR`, `--motd`, `--status-interval`, `--tag` | as the original (`--tag` is accepted and unused) |
| `--check-leaks` | accepted, does nothing (it is a Python memory diagnostic) |

Added: `--source-url URL` (the AGPL source location sent to receivers; set it to where this repository is), `--engine-threads N`, `--io-threads N`, `--no-aircraft-json`.

No port numbers are fixed in either program; the usual 31090 (receivers) and 31003 (filtered Basestation) work unchanged.

### Receiver protocol

Handshake versions 2 and 3, compression `zlib2` / `zlib` / `none`, the optional `PROXY` line, the UDP transport (`udp_transport: 2`, same packed datagram format and key), and the message types `sync`, `mlat`, `seen`, `lost`, `rate_report`, `input_connected`, `input_disconnected`, `clock_reset`, `clock_jump` and `heartbeat`. Results go back in both the `old` and `ecef` formats, with `return_stats`. Tested against recorded traffic from about 140 real receivers running mlat-client 0.4.2; other client versions have not been tested.

### Output

* **Basestation (SBS)** lines are formatted exactly as the original (`MSG,3,...`), including its filtering of poor results on the filtered port.
* **State files** in `--work-dir`: `sync.json`, `clients.json` and `aircraft.json` (optional) with the same fields, order, JSON types and rounding. mlat.uk's data pipeline, which reads `clients.json` and `sync.json`, produces the same feeder counts and links from both servers' files.
* **Status log line** and `handshakes.log` as the original. The `node_exporter` text file is written if `/run/node_exporter` exists.

### Known differences

* The order of peers in `sync.json` / `bad_peer_list` and of the interest lists follows receiver id, not hash or insertion order.
* `aircraft.json` `elapsed_seen` is larger for aircraft that only appear in rate reports.
* The `quine` debugging message and the `MLAT_CPU_PROFILE` hook of mlat.uk's build are not implemented.
* Datagrams on the UDP transport are authenticated only by the key the receiver was given and, as in the original, bypass the "no mlat from a receiver with bad sync" check that the TCP path has.

## Running

    cp example.env .env && docker compose build && docker compose up -d

See [docs/DEPLOY.md](docs/DEPLOY.md) for the compose file and every setting in `example.env` (container name, CPU limit, listeners, MOTD). To replace a running Python server without losing its state volume, see [docs/CUTOVER.md](docs/CUTOVER.md). Without Docker:

    cargo build --release -p mlat-server --features glibc-pow
    target/release/mlat-server-rs --client-listen 31090 --filtered-basestation-listen 31003 --work-dir /run/mlat-server

`glibc-pow` computes squares and cubes with the C library's `pow()`, as the Python code does. Rust's `x*x` and glibc's `pow(x, 2.0)` differ in about 1 case in 1,200, so this keeps the results closer to the original; the Docker image uses it.

## How it works

    receivers --TCP/UDP--> connection threads (tokio) --> position engine --> results
                                                              |                  |
                          interest worker <- seen/lost/rate   +- pairer threads   +- per-receiver results
                          (what each receiver is asked        |  (clock sync)     +- Basestation clients
                           to send)                           +- matcher threads  +- CSV / pseudorange files
                                                                 (tracking, mlat,
                                                                  solver, Kalman)
    clock thread: group deadlines, cleanup, 15 s health pass     state thread: sync/clients/aircraft.json

## How it was checked

During development the Rust pipeline was compared with the original at each stage (clock synchronisation, solver, normalisation, Kalman filter, grouping, SBS formatting, the whole pipeline) using values recorded from the Python code and a virtual-clock replay of real traffic, and then as a whole server against a recording of production traffic. That test tooling (capture, replay and comparison scripts) is not part of this repository.

## Feedback welcome

Most useful: results from larger installations, other client versions, and anything that differs from the original's behaviour. Please open an issue.

## Credits and licence

The algorithms and file formats are those of [mlat-server](https://github.com/wiedehopf/mlat-server) by wiedehopf, itself a fork of Oliver Jowett's [mlat-server](https://github.com/mutability/mlat-server). AGPL-3.0-or-later; see `LICENSE`.
