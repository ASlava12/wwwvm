# Deploying wwwvm on the internet

The browser can't open raw TCP sockets, so reaching the network from the
in-browser VM always goes through a small server-side relay
(`wwwvm-proxy`, a WebSocket↔TCP gateway). The chain is:

```
browser → WS relay (yours) → [optional public proxy] → target
```

You can't drop the relay — but a relay locked to a **narrow allowlist** is not
an "open relay" and is safe to expose. This guide stands up the static site and
the relay behind **single-origin TLS** with one command, using Caddy for
automatic HTTPS.

## TL;DR

```sh
# 1. Build the browser assets ON THE HOST (they're gitignored, not in the image):
cargo install wasm-pack            # once
wasm-pack build crates/wasm --target web --out-dir ../../web/pkg --release
scripts/build-web-images.sh        # guest cpios + manifest → web/images/
#    (add --with-x for the preinstalled-X desktop image; --with-gui/--with-net
#     control the others — see the script)

# 2. Bring it up (real domain → automatic Let's Encrypt TLS):
WWWVM_DOMAIN=example.com docker compose up -d --build

# 3. Open https://example.com, and in the UI set:
#      proxy ws = wss://example.com/ws
#    tick Networking, pick an image, boot.
```

DNS for `example.com` must point at the host, and ports 80+443 must be reachable
(Let's Encrypt needs 80 for the ACME challenge).

### Local test (no domain, no real cert)

```sh
docker compose up --build      # WWWVM_DOMAIN defaults to localhost
# open https://localhost (accept the internal self-signed cert),
# set proxy ws = wss://localhost/ws
```

## What the compose stack does

- **caddy** — terminates TLS (auto for a real domain; internal self-signed for
  `localhost`), serves `web/` (mounted from the host, so an asset rebuild needs
  no image rebuild), and reverse-proxies `wss://<domain>/ws` → the relay's plain
  `ws`. Page and socket share one origin, so there's no mixed-content block and
  the relay sees `Origin: https://<domain>`.
- **relay** — `wwwvm-proxy`, internal only (not published). Configured by env in
  `docker-compose.yml`.

## Security knobs (set in docker-compose.yml / env)

| Env var | Purpose | Default here |
|---|---|---|
| `WWWVM_PROXY_ALLOWLIST` | Hosts the relay may reach. **Scope this.** `*` = open relay (never on a public bind). | apk mirrors |
| `WWWVM_PROXY_ORIGINS` | Allowed WebSocket Origin(s) (anti-CSWSH). | `https://$WWWVM_DOMAIN` |
| `WWWVM_PROXY_MAX_CONNS` | Global concurrent connections (`0`=off). | 512 |
| `WWWVM_PROXY_MAX_CONNS_PER_IP` | Per-IP concurrent (`0`=off). | 16 |
| `WWWVM_PROXY_IDLE_TIMEOUT_SECS` | Close idle tunnels (`0`=off). | 120 |
| `WWWVM_PROXY_MAX_BYTES` | Cap bytes/connection (`0`=off). | off |

The relay also resolves every target itself and refuses non-globally-routable
IPs (SSRF guard), independent of the allowlist.

> Widening `WWWVM_PROXY_ALLOWLIST` beyond what the demo needs turns the relay
> into a more general tunnel. Keep it to the exact hosts (e.g. your package
> mirror). To let the guest reach the open internet, prefer routing the relay's
> egress through rotating **public** proxies (below) rather than allowing `*`.

## Hiding your egress IP (optional public-proxy upstream)

The relay can chain each connection through a third-party public proxy, so the
target sees the proxy's IP, not your server's:

```sh
scripts/fetch-proxies.py --out /path/to/proxies.json   # from cron, e.g. */30
```

Mount that file and point the relay at it (uncomment in `docker-compose.yml`):

```yaml
    environment:
      WWWVM_PROXY_UPSTREAMS_FILE: "/data/proxies.json"
    volumes:
      - /path/to/proxies.json:/data/proxies.json:ro
```

Then in the UI set **upstream proxy → Auto-rotate** (or pick/enter one). Public
proxies are untrusted and flaky — use for non-sensitive traffic only, and keep
the allowlist scoped regardless.

## Alternative: relay's built-in TLS (no Caddy)

`wwwvm-proxy` can serve `wss://` directly if you'd rather expose it without a
reverse proxy:

```sh
WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80,dl-cdn.alpinelinux.org:443' \
WWWVM_PROXY_ORIGINS='https://example.com' \
WWWVM_PROXY_TLS_CERT=/etc/ssl/fullchain.pem \
WWWVM_PROXY_TLS_KEY=/etc/ssl/privkey.pem \
  cargo run -p wwwvm-proxy -- 0.0.0.0:8443
```

Then `proxy ws = wss://example.com:8443`. (You supply the cert/key — e.g. from
certbot. Caddy is easier because it obtains and renews them for you.)
