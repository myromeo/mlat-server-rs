# Deploying with Docker

    cp example.env .env       # edit: CPUs, MOTD, source URL
    docker compose build      # compiles on this machine, so it matches its CPU (x86-64 or arm64)
    docker compose up -d
    docker compose logs -f

The compose file publishes only the receiver port 31090 (TCP and UDP). The filtered Basestation
output on 31003 is exposed on the docker network only, so readsb reads it by container name:

    --net-connector=mlat-server,31003,sbs_in

Keep `MLAT_CONTAINER_NAME=mlat-server` for that to work unchanged.

readsb has to be on the same docker network. By default the compose file uses the project's own
network (`<folder>_default`). If readsb runs in another stack, either put both services in one
compose file, or join the existing network: copy `docker-compose.network.example.yml` to
`docker-compose.override.yml` and set the network's name.

State files (`sync.json`, `clients.json`, `aircraft.json`, rewritten every 15 s) live in the
`mlat-work` volume, mounted at `/run/mlat-server`. `example.env` documents every setting; the
entrypoint turns the `MLAT_*` variables into command-line flags (anything else can be passed as
`command:`).

## CPUs

`MLAT_CPUS` (default 1) is the number of cores the container may use. The server spreads over
whatever it is given: connection handling scales with the cores available, and
`MLAT_ENGINE_THREADS` (default 1) spreads the position engine over more matcher/pairer threads,
which only matters at several times today's load. Raise `MLAT_CPUS` as stations are added.

## UDP

Set `MLAT_CLIENT_LISTEN=31090:31090`; the compose file already publishes the UDP port. Receivers
that offer the UDP transport switch to it on their next connection.

## Licence

The server is AGPL-3.0-or-later. Anyone who connects to it must be able to get its source: set
`MLAT_SOURCE_URL` to where this repository is published.
