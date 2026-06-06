# Security model & safe deployment

`wwwvm` runs an untrusted x86 guest in the browser and ships two **optional**,
self-hostable network services (`crates/proxy`, `crates/snapstore`). This file
documents the trust boundaries and the footguns to avoid when deploying. It
reflects the state after the 2026-06 security audit; see `git log` for the
individual hardening commits.

## Trust boundaries

| Boundary | Trusted? | Why it's safe |
|---|---|---|
| Guest kernel + userland (the emulated CPU) | **Untrusted** | `crates/cpu` + `crates/mem` are `#![forbid(unsafe_code)]`; all guest memory access is bounds-checked (OOB read → 0, OOB write → no-op). A malicious guest can only mislead itself or raise guest faults — it cannot read/write host memory. Worst case is a panic (tab/process DoS); the audit fixed the known guest-triggerable panics. |
| Kernel image / initramfs the user loads | **Untrusted** | `load_bzimage`/`load_elf`/cpio parsers bounds-check header fields; a crafted image is rejected (`BzImageError`) rather than panicking. |
| Snapshot blob (`.bin`, snapstore, shared training snapshots) | **Untrusted** | `restore`/`restore_export`/`decode_export` validate every length/offset/count/version before indexing; device records are masked/clamped. A malformed snapshot errors, it does not panic or over-allocate. **Page content from a store is length-checked but not blake3-verified in JS** (no JS blake3) — see "snapstore" below. |
| `crates/proxy` relay | Operator-configured | Deny-by-default allowlist; resolved-IP SSRF pinning; Origin lock; refuses to start as an open relay on a public bind. |
| `crates/snapstore` | Operator-configured | Bearer-token writes (fail-closed, constant-time); content-addressed + blake3-verified pages; path-traversal-safe ids. |

## The browser front-end (`web/`)

- **No server is needed** for the in-browser features (single VM, graphics,
  peer-to-peer Fleet LAN, local snapshots). Serve the static files over HTTPS.
- Guest serial output is written to xterm (`term.write`), **never** to
  `innerHTML` — there is no XSS path from guest output, manifests, or DNS.
- The `.wasm` must be served as `Content-Type: application/wasm`.
- **There are no secrets in the repo.** The snapstore admin token and any relay
  config are supplied at runtime via env vars; never commit them.

## `crates/proxy` (WebSocket ↔ TCP relay) — the highest-risk surface

The relay lets the in-wasm NAT reach the real internet. Misconfigured, it is an
open relay / SSRF vector. Rules:

1. **Never run with `WWWVM_PROXY_ALLOWLIST='*'` on a reachable bind.** `*` is an
   open relay. The proxy now **refuses to start** in that configuration unless
   `WWWVM_PROXY_I_REALLY_WANT_AN_OPEN_RELAY=1` is set. Use a *specific* allowlist
   (`dl-cdn.alpinelinux.org:80`), and/or bind to `127.0.0.1`.
2. **Set `WWWVM_PROXY_ORIGINS`** to the exact page origin(s) (scheme+host+port,
   comma-separated) to block Cross-Site WebSocket Hijacking. Unset = any Origin
   (loopback dev only). Note `localhost` ≠ `127.0.0.1` ≠ your LAN IP — list each
   origin you actually open the page from.
3. **Direct mode is SSRF-pinned**: the relay resolves the host itself and
   connects to the pinned globally-routable IPv4 (internal/loopback/link-local/
   CGNAT/IPv6 are refused), so DNS-rebinding and IP-literal tricks don't work.
4. **Upstream/auto mode delegates the target to a third-party proxy**, so the
   IP pin does not apply — only the allowlist name check does. Treat
   upstream/auto as "trust the chosen upstream"; do not combine it with `*`.
5. For an HTTPS page the relay must be **`wss://`** (TLS) — a browser blocks
   `ws://` from `https://` as mixed content. Serve TLS directly
   (`WWWVM_PROXY_TLS_CERT`/`_KEY`) or behind a TLS reverse proxy.
6. Public proxies fed via `proxies.json`/upstream are **untrusted** — route only
   non-sensitive traffic, never credentials.

## `crates/snapstore` (content-addressed snapshot store)

- **Writes require a bearer token** (`WWWVM_SNAPSTORE_TOKEN`); unset ⇒ read-only
  (every `PUT` → 401, fail-closed). The compare is constant-time. An
  unauthorized `PUT` is rejected **before** its body is read.
- Pages are content-addressed and **blake3-verified on write** — a valid token
  still cannot poison a hash.
- **Reads are open by design.** If a snapshot is sensitive, anyone with the
  URL/id can read its pages/manifest — put the store behind auth or a private
  network if that matters.
- DoS hardening: per-read idle timeout + a concurrent-connection cap. Still,
  front it with a reverse proxy / rate limiter for a public deployment.
- The browser client length-checks downloaded pages but **cannot verify their
  blake3** (no JS blake3). A hostile store could serve wrong page bytes that
  land in *guest* RAM (guest-integrity only, not host code) — so **point the UI
  at a store you trust**.

## Reporting

This is an educational project. If you find a memory-safety issue (a way for a
guest or a crafted image/snapshot to read/write host memory, not just panic),
that's the highest-value report — please open an issue with a repro.
