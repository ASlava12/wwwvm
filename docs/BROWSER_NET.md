# Networking in the browser (wasm)

**Status: foundation in place, WebSocket relay is the remaining piece.**

The native build reaches the internet via `crates/net` — a smoltcp TCP NAT
that opens real `std::net::TcpStream`s on background `std::thread`s (see
`docs/NET_BRIDGE_DESIGN.md`). **None of that runs in a browser**: wasm32 has
no raw TCP sockets and no threads. So browser networking takes a different
shape, reusing the parts that *are* portable.

## Architecture

```
guest TCP/IP ──L2 Ethernet frames──>  wwwvm-wasm (the VM, in the browser)
                                           │  drain_tx_frame() / inject_rx_frame()
                                           ▼
                                  browser bridge (JS or wasm)   ← TO BUILD
                                     · terminates the guest's TCP (L2↔L4 NAT)
                                     · one WebSocket per guest connection
                                           │  {"host","port"} + binary bytes
                                           ▼
                                  wwwvm-proxy  (WebSocket ↔ TCP, allowlisted)
                                           │
                                           ▼
                                     real server (e.g. dl-cdn.alpinelinux.org)
```

The guest emits **L2 Ethernet frames**; the proxy speaks **L4 TCP** (a JSON
connect frame `{"host":"…","port":N}` then raw bytes both ways, see
`crates/proxy`). Something must bridge L2→L4 — that's the NAT, and in the
browser it lives above the wasm frame API.

## What's done

`crates/wasm` now exposes the NIC frame stream (the native build's
`drain_tx_frames`/`inject_rx_frame`, plus an idle-aware step):

```js
vm.run_until_idle(maxSteps);          // step until the guest blocks on I/O
let f;                                 // drain everything the guest sent
while ((f = vm.drain_tx_frame())) bridge.onGuestFrame(f);   // Uint8Array
const accepted = vm.inject_rx_frame(frame);   // false ⇒ ring full, retry next tick
```

This is the necessary seam; with it the VM core needs no further change.

## What remains (the bridge)

Two ways to implement the L2↔L4 NAT in the browser:

- **(A) smoltcp-in-wasm (recommended, max reuse).** Compile `crates/net`'s
  `nat.rs` + `device.rs` to wasm and replace `relay.rs` (threads + std sockets)
  with a **web-sys `WebSocket`** relay: when the NAT accepts a SYN, open a
  WebSocket to the proxy, send the connect frame, then pipe the byte stream
  through the per-connection channels. The NAT logic, the catch-all SYN
  handling, the DNS forwarder, and the allowlist all carry over unchanged.
  smoltcp itself is wasm-friendly; the time source comes from `Date.now()`.
  `relay.rs` would become `#[cfg(not(target_arch = "wasm32"))]` with a wasm
  twin.
- **(B) JS NAT.** Parse ARP/IPv4/TCP in JS and NAT to WebSockets. More code,
  no Rust reuse — not recommended.

The proxy already exists and is exactly this gateway; deploy it with a
**specific** allowlist, never `*`:

```
WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:443' cargo run -p wwwvm-proxy -- 0.0.0.0:9000
```

## Testing

The end-to-end path needs a real browser + a running proxy, so it can't be
teeth-confirmed headless like the native milestones. The NAT logic is already
covered by the native `wwwvm-net` unit/integration tests; the browser-specific
work to verify is the WebSocket relay + the `Date.now()` time source.
