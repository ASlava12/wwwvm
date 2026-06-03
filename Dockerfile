# syntax=docker/dockerfile:1
# Build & run the WebSocket↔TCP relay (wwwvm-proxy).
#
# This image is ONLY the relay. The browser-served static site (web/) is served
# by Caddy from a host mount in docker-compose.yml, not baked in here — so the
# wasm bundle / guest images don't need rebuilding to ship a relay change.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
# Only the relay crate (+ its dep wwwvm-net) and their crates.io deps compile.
RUN cargo build --release -p wwwvm-proxy

FROM debian:stable-slim
# Run unprivileged.
RUN useradd -r -u 10001 -m wwwvm
COPY --from=build /src/target/release/wwwvm-proxy /usr/local/bin/wwwvm-proxy
USER wwwvm
EXPOSE 8080
ENTRYPOINT ["wwwvm-proxy"]
# Bind inside the container; Caddy (or you) publishes it. Override as needed.
CMD ["0.0.0.0:8080"]
