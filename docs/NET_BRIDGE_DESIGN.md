# Guest↔internet bridge design (networked apk)

Design produced by a multi-agent design workflow (4 design angles → synthesis →
adversarial critique), 2026-05-31. This is the implementation plan for **Net B/C/D**:
letting the Alpine guest reach a real Alpine mirror so `apk update` / `apk add` work
over the network. A/B1 (NIC + ARP/ICMP `VirtualGateway`, `ping 10.0.2.2`) are already
done and teeth-confirmed.

## Big decisions

**(a) TCP engine → smoltcp** (new `crates/net` crate), hand-rolled NAT only as a
documented fallback. The correctness risk (seq/ack modular arithmetic, SYN/FIN seqno
consumption, windows, retransmit, TIME_WAIT) lives entirely in the part smoltcp already
solved, and its bugs would present as "hangs on large transfers, passes small-file
tests" — the worst profile for a teeth-confirmed project. smoltcp is
`#![forbid(unsafe_code)]`-compatible; only `crates/net` takes the dep, so `vm`/`cpu`/`mem`
stay pure-safe.

  - **Catch-all destination** (a NAT must accept the guest's SYN to *any* dst IP:port):
    `iface.set_any_ip(true)` + sniff the guest SYN + lazily
    `socket.listen(IpListenEndpoint{ addr: Some(dst_ip), port: dst_port })` on the exact
    destination, *then* enqueue the SYN into the device so `poll()` completes the
    handshake. **Listen BEFORE poll for that frame** (critique B1) or smoltcp silently
    drops the SYN (no RST) and apk hangs. Pre-seed the neighbour cache with the guest MAC
    `52:54:00:12:34:56`, add the guest /24, set a default route.

**(b) Concurrency → VM stays single-threaded; a background tokio current-thread runtime
owns the host sockets; bytes cross via PER-CONNECTION bounded channels.** The VM thread
calls `iface.poll()` inline each loop turn (smoltcp touches no host socket) and only ever
does non-blocking `try_send`/`try_recv`. Relay tasks run on the tokio thread via
`Handle::spawn` (**`rt.spawn`, never bare `tokio::spawn`** — critique C3).

  - **Do NOT use a single shared host→NAT channel** (critique C1): one shared channel
    head-of-line-blocks independent flows and can deadlock two interdependent apk
    connections. Use per-connection channels both directions; a shared channel may carry
    only small control events, with data via per-conn channels the NAT drains round-robin.

**(c) DNS + bootstrap → in-gateway DNS on `10.0.2.2:53`, static IP, HTTP for the MVP.**
DNS is a `handle_frame` arm (UDP/53 → gw_ip), not a real host UDP socket. **Resolution
must be async** (critique E1): `std::net::ToSocketAddrs` blocks → would freeze the
single-threaded VM; send the resolve to the tokio thread and inject the DNS *response*
frame later via the egress path (apk has its own DNS timeout).

  - Gate DNS by name (`permits_host`); record IP→{names} so the TCP NAT can recover the
    hostname for the allowlist at SYN time. Non-allowlisted → NXDOMAIN. AAAA → empty
    NOERROR (32-bit guest, no v6). musl sends A+AAAA in parallel; match responses by
    txn-ID + question, echo the question verbatim, guard QNAME pointer-loops, reject
    QDCOUNT≠1, refuse ANY/TXT (exfil vectors).
  - **HTTP, not HTTPS, for the MVP**: the minirootfs ships no CA bundle and can't
    `apk add ca-certificates` before networking exists (chicken-and-egg). apk
    authenticates via RSA APKINDEX signatures (`/etc/apk/keys/*.rsa.pub` — **confirmed
    present** in `/tmp/alpine/modroot/etc/apk/keys/`), so plain HTTP is still
    authenticated. HTTPS is Phase D (bake the host CA bundle; transparent relay needs no
    change since TLS terminates in-guest — that's where the no-MITM property is shown).
  - Bootstrap by **editing the generated `/init` string** + injecting `/etc/resolv.conf`;
    `/etc/apk/repositories` already exists in the rootfs (overwrite, don't append a
    duplicate cpio entry — critique H5). Static: `ip addr add 10.0.2.15/24`,
    `ip route add default via 10.0.2.2`, `nameserver 10.0.2.2`, repo `http://dl-cdn…`.

**(d) Allowlist → shared module, checked twice, deny-by-default.** Lift
`Allowlist`/`AllowEntry` from `crates/proxy` into `crates/net/src/allow.rs` (one impl;
proxy depends on it). Check `permits_host(name)` at DNS-resolve and `permits(host, port)`
at SYN (host recovered from the IP→names map). Denied SYN → `socket.abort()` (RST, no
hang). Empty env = deny-all. **`*` is never the shipped default.** Run config:
`WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80,dl-cdn.alpinelinux.org:443'`.

  - **SSRF/DNS-rebinding hardening** (critique F1): at connect time re-validate the
    resolved IP is not private/loopback/link-local/multicast/own-subnet; store a *set* of
    names per IP; TTL the map so stale entries can't authorize later.

## Phased plan (each phase ends in a teeth-confirmable in-guest milestone)

- **B1 — `crates/net` skeleton + shared allowlist.** New `wwwvm-net` crate (deps:
  smoltcp 0.11+ `default-features=false, features=["std","medium-ethernet","proto-ipv4",
  "socket-tcp","socket-udp"]`; tokio). Move `Allowlist` + tests in (rewrite `is_none_or`
  → `map_or(true,…)`, or bump workspace `rust-version`; MSRV 1.75 is already fiction —
  toolchain 1.95, proxy uses 1.82+ `is_none_or`). Confirm: `cargo test -p wwwvm-net`,
  proxy still builds. Pure refactor.
- **B2 — DNS forwarder.** `dns.rs`: `parse_question`, `build_response`, async resolve via
  tokio. Confirm: in-guest `nslookup dl-cdn.alpinelinux.org 10.0.2.2` → A record;
  `gibberish` → NXDOMAIN.
- **B3a — TCP handshake + relay MVP.** `nat.rs`: `GuestDevice` (smoltcp `phy::Device`,
  Ethernet, MTU 1500, **no FCS either way**, may be <60B — critique A2), `NatStack`
  (`push_guest_frame`/`poll`/`next_egress_frame`/`requeue`/`poll_at`). Driver-loop change
  in alpine_console: egress injection **top-level/unconditional** (NOT nested under TX
  drain — critique B3 BLOCKER), requeue-to-**front** (B2), inject budget (~8/turn), reduce
  `chunk` when a connection is active (C2). Confirm: in-guest `apk update` over HTTP.
- **B3b — robustness.** Half-close ordering, concurrent flows, TIME_WAIT, retransmit
  timing, sustained backpressure. Confirm: `apk add tree && tree --version`.
- **D — HTTPS.** Bake host CA bundle into the cpio; flip repo to `https://`, add `:443`.
  No relay change. Confirm: `apk add` over HTTPS (proves end-to-end TLS / no MITM).

## Blockers / majors to fix before/while coding (from the critique)

- **BLOCKER B3:** egress injection must be top-level, not nested under `drain_tx_frames`
  (the existing loop nests RX injection inside the TX-drain `for`).
- **Major B1:** listen-before-poll ordering; size smoltcp `SocketSet` + per-socket buffers
  for ≥4–8 concurrent flows in B3a.
- **Major B2:** egress over-production vs the 8 KB lossy RX ring; requeue-to-front + don't
  pull new egress while a frame is stuck (else reordering → dup-ACK storms).
- **Major C1:** no single shared host→NAT channel (head-of-line deadlock).
- **Major C3:** `rt.spawn`, not `tokio::spawn`, from the VM thread.
- **Major E1:** DNS resolve must not block the VM thread (`ToSocketAddrs` is blocking).
- **Major E2:** robust DNS parse (musl A+AAAA, ID+question match, pointer-loop guard).
- **Major F1:** DNS-rebinding/SSRF — re-validate connect IP not private; names-set per IP;
  TTL.
- **Major G1:** smoltcp must be the single ARP authority for 10.0.2.2 (it needs ARP for
  TCP). `lan.rs` ARP/ICMP and smoltcp can't coexist on one IP — migrate ARP+ICMP+TCP to
  smoltcp in one step and **re-run the `ping 10.0.2.2` milestone as a regression gate**.
- **Major A1:** MSRV vs `is_none_or`/smoltcp — resolve when lifting the allowlist.
- Minors: TX frames FCS-less/<60B (A2); example already uses `unsafe` so keep the loop in
  the binary not `crates/vm/src` (A3); `inject_rx_frame==false` also means RX-disabled,
  bound requeue (B4); RX ring size is guest-chosen via RCR, don't assume (H1); `>=` drop
  threshold, use `len − max_frame` headroom (H2); per-byte DMA is slow for multi-MB (H3);
  `FiveTuple` is really a 4-tuple (H4); overwrite cpio files don't dup (H5).
