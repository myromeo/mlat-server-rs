# mlat-server-rs: build on the target machine, so the binary matches its CPU
# architecture (x86-64 VPS or arm64 alike):  docker compose build
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# glibc-pow: use libm's pow() where the Python server does, for matching results
RUN cargo build --release --locked -p mlat-server --features glibc-pow \
    && cp target/release/mlat-server-rs /mlat-server-rs

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends tini ca-certificates util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home mlat \
    && mkdir -p /run/mlat-server && chown mlat /run/mlat-server
COPY --from=build /mlat-server-rs /usr/local/bin/mlat-server-rs
COPY deploy/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
# starts as root only to make the state directory writable (a volume left by the Python
# server is owned by root), then drops to the unprivileged user
VOLUME /run/mlat-server
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
