# Networking in the browser (wasm)

**Status: implemented (option A) — needs a real browser + running proxy to
confirm end-to-end.** The smoltcp TCP NAT now runs *inside* wasm with a
thread/socket-free connector ([`QueueConnector`](../crates/net/src/queue.rs));
JS tunnels each guest flow over a WebSocket to `crates/proxy`. The NAT logic is
unit/loopback-tested natively (`crates/net`); the WebSocket relay + DoH
resolver are the browser-only parts.

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

## How it works (option A — smoltcp-in-wasm)

The NAT runs in wasm; only the per-connection *transport* changed. The native
relay backs each flow with a `std::thread` + `TcpStream` (impossible in wasm),
injected via the NAT's existing `Connect` seam. The browser swaps in
[`QueueConnector`](../crates/net/src/queue.rs): it builds the same
`HostConn{to_host, from_host, stop}` the NAT consumes, but retains the *opposite*
ends of those channels in a registry. The NAT is byte-for-byte unchanged; JS
drains/fills the registry and shuttles bytes over a WebSocket.

`crates/wasm` exposes:

```js
vm.net_enable("dl-cdn.alpinelinux.org:80");   // deny-by-default allowlist
vm.net_cache_dns("dl-cdn.alpinelinux.org", packedIPv4);  // JS pre-resolves (DoH)
// each tick, after stepping the CPU:
vm.net_pump(performance.now());               // VM NIC frames ⇄ smoltcp NAT
for (const c of JSON.parse(vm.net_take_new_connections())) openWebSocket(c); // {id,host,ip,port}
const out = vm.net_conn_outbound(id);         // Uint8Array | undefined(=closed) → ws.send
const ok  = vm.net_conn_send(id, wsBytes);    // host→guest; false ⇒ re-queue & retry
vm.net_conn_closed(id);                        // ws closed/errored → guest FIN
```

`net_pump` bridges the VM's NIC frame stream (`drain_tx_frames` /
`inject_rx_frame`) into the in-wasm NAT itself, so JS only handles the
per-connection WebSocket payload + DNS — it never parses Ethernet/IP/TCP. The
demo wiring lives in `web/main.js` (`pumpNet` / `netOpenConn` / `netPreResolve`).

DNS: the browser can't do raw DNS, so `netPreResolve` resolves each allowlisted
host via **Cloudflare DoH** and seeds `net_cache_dns` before the guest queries.

The proxy is the gateway; deploy it with a **specific** allowlist, never `*`
(an open relay is dangerous). It receives the *hostname* (re-resolves + pins, so
a poisoned wasm-side DNS answer can't redirect it to an internal IP):

```
WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80' cargo run -p wwwvm-proxy -- 0.0.0.0:8080
```

## Testing

The byte path is teeth-tested natively: `queue::tests::guest_tcp_through_queue_nat_echoes`
drives a real guest-side smoltcp TCP client THROUGH the NAT (handshake + data
both ways) with the `QueueConnector` as the echoing host. The end-to-end
browser path (WebSocket relay + DoH + `performance.now()` time source) needs a
real browser + a running proxy and can't be confirmed headless.
