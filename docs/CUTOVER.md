# Cut-over from the Python server

Receivers need no change: they reconnect to the same host and port, and readsb keeps reading the
same Basestation port.

## 1. Shadow run (optional, recommended)

Bring the new server up next to the old one on a spare port, with its own container name and
volume, before it takes real traffic. In a copy of the folder with its own `.env`:

    MLAT_CONTAINER_NAME=mlat-server-rs   MLAT_CLIENT_PORT=41090   docker compose -p mlat-shadow up -d --build

Point a test receiver at port 41090 and check: `Handshake successful` in `docker compose logs`,
the receiver in `clients.json` (`docker exec mlat-server-rs cat /run/mlat-server/clients.json`),
Basestation lines on 31003 from inside the network, and `docker stats` for CPU. Then
`docker compose -p mlat-shadow down`.

## 2. Cut over

1. `docker compose build` the new image next to the running Python container.
2. Note the Python container's compose service, so it can be restarted.
3. Stop the Python container and start the new one on the production ports
   (`docker compose up -d`). Receivers were told `reconnect_in` ~10 s, so they are back within
   about 20 s; the first positions follow within a minute while clocks resynchronise.
4. Check:
   - `docker compose logs -f`: `Handshake successful` lines and a `Status:` line every interval
   - `work/clients.json` lists about the same number of receivers as before
   - readsb receives Basestation lines (readsb's own logs or `docker exec readsb ...`; 31003 is not published to the host)
   - `docker stats`: CPU well below one core, memory flat
5. Roll back by stopping the new container and starting the old one: nothing else changed.

## 3. After a week

Compare `positions.csv` volume and the number of aircraft in `aircraft.json` with a day from the
Python era, and confirm the status line's outlier percentage is no worse.
