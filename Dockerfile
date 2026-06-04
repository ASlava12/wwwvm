# syntax=docker/dockerfile:1
# Build the two server binaries — the WebSocket↔TCP relay (wwwvm-proxy) and the
# snapshot store (snapstore-server). docker-compose runs each as a service from
# this one image (pick the binary via `command`). The browser-served static site
# (web/) is served by Caddy from a host mount, not baked in here — so the wasm
# bundle / guest images don't need rebuilding to ship a server change.
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p wwwvm-proxy -p wwwvm-snapstore

FROM debian:stable-slim
# Run unprivileged.
RUN useradd -r -u 10001 -m wwwvm
COPY --from=build /src/target/release/wwwvm-proxy /usr/local/bin/wwwvm-proxy
COPY --from=build /src/target/release/snapstore-server /usr/local/bin/snapstore-server
USER wwwvm
# Default to the relay; the snapstore service overrides `command` in compose.
EXPOSE 8080
CMD ["wwwvm-proxy", "0.0.0.0:8080"]
