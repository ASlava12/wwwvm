# wwwvm host networking bridge — concurrency / integration design

Scope: how the **synchronous, single-threaded** VM step loop and **blocking/async
host sockets** coexist so `apk update` / `apk add` in the guest reaches a real Alpine
mirror, with a transparent (no-MITM) TCP relay gated by an allowlist.

This doc is about the **integration/concurrency layer only**. It is deliberately
agnostic to whether the TCP-translation engine is `smoltcp` or hand-rolled — that
engine is named here as a trait-bounded component (`TcpEngine`) and its internals are
out of scope.

---

## 1. The constraint we are designing around

From `crates/vm/src/lib.rs` the VM exposes exactly three relevant calls, all `&mut self`:

```rust
impl Vm {
    pub fn run_steps_idle_aware(&mut self, max: u32) -> (u32, Stop);
    pub fn drain_tx_frames(&mut self) -> Vec<Vec<u8>>;   // guest -> host, L2 frames (no CRC)
    pub fn inject_rx_frame(&mut self, frame: &[u8]) -> bool; // host -> guest; false = dropped (RX off / ring full)
}
```

Key facts that shape the design:

- `Vm` owns `Cpu + Memory + IoBus`. It takes `&mut self` everywhere and is **not** safe
  to assume `Send`/`Sync`. The VM therefore stays pinned to **one** thread — the thread
  that already runs the boot/console loop (`crates/vm/examples/alpine_console.rs`).
- `inject_rx_frame` can **silently drop** (`-> false`) when the RTL8139 RX ring is full
  or RX is disabled (`crates/devices/src/rtl8139.rs::accept_rx -> Option<(dest, bytes)>`).
  So the host→guest direction already has lossy backpressure; the design must treat a
  dropped inbound frame as normal (TCP will retransmit) and must re-offer it later rather
  than assume delivery.
- The existing loop is the canonical shape (alpine_console.rs lines 165-184):
  ```text
  loop {
      run_steps_idle_aware(chunk);
      for f in drain_tx_frames() { for r in gateway.handle_frame(f) { inject_rx_frame(r) } }
      drain UART; feed stdin;
  }
  ```
  The whole networking subsystem must slot into this loop **without ever blocking it** —
  the console must stay responsive even when a socket is mid-`connect()` to a slow mirror.

The conclusion that drives everything below: **the VM thread must never touch a host
socket.** All blocking/async I/O lives on a second thread. The two threads exchange only
plain owned byte buffers (`Vec<u8>` L2 frames) over `std::sync::mpsc` channels, plus a
tiny control enum. No shared `Vm`, no locks around CPU/Memory.

---

## 2. Two threads, frames over channels

```
                 main / VM thread (single-threaded, owns Vm)            net thread (owns sockets + TcpEngine)
                 ------------------------------------------            -------------------------------------
   run_steps_idle_aware(chunk)
   ─ drain_tx_frames() ───────────► to_net: Sender<FromVm>  ─────────► rx.recv() / try_iter()
                                                                        engine.on_guest_frame(frame)
                                                                          ├─ ARP / ICMP  -> reply frame
                                                                          ├─ DNS query   -> resolve, A-record reply
                                                                          └─ TCP segment -> NAT to a host TcpStream
                                                                              (allowlist-gated at SYN)
   ◄── inject_rx_frame(f) ◄──────── from_net: Receiver<ToVm> ◄────────  engine emits reply/relayed frames
   (re-offer on `false`)
```

- **`to_net` (VM → net):** every frame from `drain_tx_frames()` is forwarded verbatim.
  Unbounded `mpsc::channel` is fine and *preferred*: the VM side must never block on
  `send`. The guest's own TCP window + the NIC TX path already rate-limit how fast the
  guest emits, so this queue is naturally bounded by the guest. `send` returning `Err`
  only happens if the net thread died — treat that as "networking down", keep booting.
- **`from_net` (net → VM):** frames the engine wants delivered to the guest. Also
  `mpsc`. The VM drains this each loop iteration and calls `inject_rx_frame`. Because
  injection can fail, the VM side keeps a small **re-offer buffer** (below).

### Message types

```rust
// crates/vm/src/net/mod.rs  (new module; types are plain data, all Send)

/// VM thread -> net thread.
enum FromVm {
    /// One L2 Ethernet frame the guest transmitted (from drain_tx_frames()).
    GuestFrame(Vec<u8>),
    /// Coarse wall-clock tick so the engine can age TCP timers / retransmits
    /// without its own clock thread. Sent once per VM loop iteration.
    Tick { elapsed_ms: u32 },
    /// VM is shutting down (guest halted / Ctrl-C). Net thread drains + exits.
    Shutdown,
}

/// net thread -> VM thread.
enum ToVm {
    /// One L2 Ethernet frame to hand to inject_rx_frame().
    GuestFrame(Vec<u8>),
    /// Diagnostics only (allowlist denial, DNS failure, connect error). The VM
    /// thread logs these to stderr; they never touch the guest console stream.
    Note(String),
}
```

Both enums are trivially `Send` (owned `Vec<u8>` / `String` / `Copy`), so the channel
endpoints cross the thread boundary cleanly without `Vm` ever needing to be `Send`.

### The per-iteration handshake (non-blocking on both ends)

VM thread, replacing the inline `gateway.handle_frame` block in alpine_console.rs:

```rust
let (steps, stop) = vm.run_steps_idle_aware(chunk);

// 1. push everything the guest sent (never blocks)
for f in vm.drain_tx_frames() {
    let _ = net.to_net.send(FromVm::GuestFrame(f)); // Err only if net thread gone
}
let _ = net.to_net.send(FromVm::Tick { elapsed_ms: loop_dt_ms });

// 2. re-offer any frame a previous iteration could not inject, then drain new ones
//    (try_iter() never blocks — returns immediately when the queue is empty)
net.reoffer.retain(|f| vm.inject_rx_frame(f) == false); // keep the ones still rejected? see note
for msg in net.from_net.try_iter() {
    match msg {
        ToVm::GuestFrame(f) => { if !vm.inject_rx_frame(&f) { net.reoffer.push(f); } }
        ToVm::Note(s)       => eprintln!("\r\n[wwwvm net] {s}\r\n"),
    }
}
```

Re-offer policy: `inject_rx_frame == false` means the RX ring is momentarily full. Keep
the frame in a **bounded** `reoffer: VecDeque<Vec<u8>>` (cap ~64) and retry it next
iteration *before* draining new frames, preserving order. If the cap is exceeded, drop
the **oldest** — TCP retransmits, and we must not let a wedged guest grow host memory
without bound. (Implementation detail: the `retain` line above is illustrative; the real
code re-offers front-to-back and stops at the first rejection so ordering is preserved.)

Neither side ever blocks: VM uses `send` (unbounded) + `try_iter` (non-blocking drain);
the net thread blocks only on its **own** socket/timer event loop, never on the VM.

---

## 3. Where `VirtualGateway` (lan.rs) lives

`VirtualGateway` (`crates/vm/src/lan.rs`) is already the ideal shape: pure
`handle_frame(&[u8]) -> Vec<Vec<u8>>`, no sockets, unit-tested with hand-built packets.

**It moves into the net thread**, as the L2/L3 front of the engine — it does not stay in
the VM loop. Rationale:

- Once we add DNS + TCP NAT, the guest's frames need responses that depend on **host
  socket state** (a TCP segment's reply depends on a real `TcpStream`). The decision
  "is this frame ARP/ICMP/DNS/TCP?" belongs next to the thing that can service TCP, so
  there is a **single** frame classifier on the net side. Splitting it (ARP in the VM
  loop, TCP in the net thread) would mean two places parse Ethernet/IP headers and two
  places own the guest's MAC/IP — a correctness hazard.
- It keeps the VM loop trivial: forward bytes, inject bytes. No protocol logic on the
  hot path that also drives the CPU and console.

Concretely, `VirtualGateway` becomes a field of the engine and `handle_frame` is the
engine's fast-path for the protocols it already owns:

```rust
struct NetEngine<S: HostNet> {
    gateway: VirtualGateway,          // ARP + ICMP echo (unchanged, lan.rs)
    dns: DnsResponder,                // answers guest DNS, records host->IP map
    tcp: TcpEngine<S>,                // smoltcp OR hand-rolled; NATs guest TCP <-> S::TcpConn
    allow: Allowlist,                 // reused from proxy crate (see §5)
    guest_mac: [u8; 6],
    guest_ip: [u8; 4],
    gw_ip: [u8; 4],
    gw_mac: [u8; 6],
}

impl<S: HostNet> NetEngine<S> {
    /// Pure-ish: frame in, frames out. The only impurity is S (host sockets),
    /// which is a trait so tests inject a fake. Mirrors lan.rs's testability.
    fn on_guest_frame(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        // 1. ARP / ICMP -> VirtualGateway (already correct, checksums verified)
        let replies = self.gateway.handle_frame(frame);
        if !replies.is_empty() { return replies; }
        // 2. UDP/53 to gw_ip -> DnsResponder (records the host name for the allowlist)
        if let Some(r) = self.dns.handle_frame(frame) { return r; }
        // 3. IPv4/TCP -> NAT engine (SYN is allowlist-gated, see §5)
        self.tcp.on_guest_frame(frame, &mut self.allow, &self.dns)
    }

    /// Drain bytes that arrived on host sockets since last poll, turned back
    /// into guest-bound L2 frames (TCP payload -> segments addressed to guest).
    fn poll_host(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        self.tcp.poll(now_ms)
    }
}
```

`VirtualGateway` itself is **unchanged** — its tests in `lan.rs` keep passing. We only
relocate where it's instantiated (net thread instead of the example's `main`).

---

## 4. The net thread's own event loop

The net thread owns the sockets and the `NetEngine`. Two viable shapes; both keep the
VM loop non-blocking. Recommended: **a tokio current-thread runtime on this one thread**,
because the transparent relay (§6) is naturally a set of `TcpStream` copy tasks and the
proxy crate already uses tokio — same idioms, same allowlist code.

```rust
// crates/vm/src/net/thread.rs
pub fn spawn(allow: Allowlist, cfg: NetConfig)
    -> (Sender<FromVm>, Receiver<ToVm>)
{
    let (to_net_tx, to_net_rx) = std::sync::mpsc::channel::<FromVm>();
    let (from_net_tx, from_net_rx) = std::sync::mpsc::channel::<ToVm>();
    std::thread::Builder::new().name("wwwvm-net".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        rt.block_on(net_main(to_net_rx, from_net_tx, allow, cfg));
    }).unwrap();
    (to_net_tx, from_net_rx)
}
```

`net_main` is a `select!` over:
- the `std::sync::mpsc::Receiver<FromVm>` — wrapped so it integrates with async. Since
  `std::sync::mpsc` isn't awaitable, use a tiny **bridge**: a blocking
  `tokio::task::spawn_blocking` loop that `recv()`s `FromVm` and forwards into a
  `tokio::sync::mpsc` channel the `select!` can await. (Alternative: skip std mpsc and
  use `tokio::sync::mpsc` end-to-end, but the VM thread is not async, so it would call
  `blocking_send`/`try_send`; std mpsc keeps the VM side dependency-free and obviously
  non-blocking. Prefer std mpsc + bridge.)
- a periodic `tokio::time::interval` (e.g. 5 ms) that drives `engine.poll_host` and TCP
  timers even when no `FromVm` arrives.
- the per-connection relay tasks signalling that upstream bytes are ready.

Each iteration: feed any `FromVm::GuestFrame` to `engine.on_guest_frame`, collect the
returned frames **plus** `engine.poll_host`, and `from_net_tx.send(ToVm::GuestFrame(_))`
each one. On `FromVm::Shutdown`, abort relay tasks, drain, return.

> Why not pure blocking threads (no tokio)? Also fine: one thread per upstream
> connection doing blocking `read`/`write`, plus a coordinator owning the engine and
> selecting via a `recv_timeout`. But the relay's "two-way copy until either side
> closes" is exactly `tokio::io::copy_bidirectional` / the proxy's `tokio::join!`
> pattern, so tokio reuses proven code. The trait boundary in §7 means tests don't pay
> for tokio anyway.

---

## 5. Where the allowlist check happens, and how denial reaches the guest

**Reuse `crates/proxy/src/main.rs::Allowlist` verbatim** — its semantics are exactly
what we need and are well-tested:
- `from_env()` reads `WWWVM_PROXY_ALLOWLIST` (comma-separated `host:port`, `host:*`, `*`).
- `permits(host, port) -> bool`, deny-by-default (empty list denies everything).
- Hard security rule preserved: **`*` is never the default.** `from_env` on an unset var
  yields an empty list (deny all), and the net thread logs the same "empty allowlist —
  every connect rejected" warning the proxy emits. `*` only ever appears if an operator
  explicitly sets it.

Action item: **lift `Allowlist` + `AllowEntry` into a shared module** so both the proxy
binary and the in-process net engine use one implementation (e.g. move them to a small
`wwwvm-net-policy` crate, or `pub` them from a shared location and have the proxy import
them). Do not copy-paste — the tests in proxy's `main.rs` should move with the type so the
deny-by-default invariants are pinned in one place.

**Where the check fires:** at the moment the engine sees the guest's TCP **SYN** for a
new flow. The destination IP is in the IP header; the **host name** comes from the DNS
query the guest made just before (apk resolves `dl-cdn.alpinelinux.org` first). The
`DnsResponder` records `ip -> hostname` for every answer it hands out, so at SYN time the
engine maps the SYN's destination IP back to the name the guest asked for and calls
`allow.permits(hostname, dst_port)`.

- This is what makes TLS work end-to-end **and** keeps the allowlist meaningful: we gate
  on the **name the guest intended**, even though the bytes are an opaque TLS stream. We
  never see the SNI or decrypt anything.
- Edge case — guest connects to a literal IP with no prior DNS (apk normally doesn't):
  fall back to `allow.permits(&dotted_ip_string, port)`. An operator who wants to allow a
  raw IP lists it as `1.2.3.4:443`. Deny-by-default still holds.

**How a denied connection is signaled to the guest:** send a **TCP RST** in response to
the SYN (engine emits a `RST`-flagged segment addressed to the guest, source = the
intended destination IP:port). Rationale:
- RST gives apk an immediate, correct "connection refused" — clean error message, no
  long timeout. Dropping the SYN would make the guest retransmit for ~minutes before
  failing, a terrible UX and it ties up the reoffer buffer.
- Do **not** use ICMP admin-prohibited: musl/apk handle TCP RST far more predictably than
  ICMP error mapping, and we already synthesize TCP segments for the relay.
- Also emit `ToVm::Note(format!("denied {host}:{port} — not in allowlist"))` to stderr so
  the operator sees why apk failed. Never write to the guest console stream.

DNS gating: the `DnsResponder` answers queries for any name (so the guest can resolve),
but resolution alone opens nothing — the gate is the TCP SYN. Optionally, only resolve
names whose A-record would be allowlisted, but resolving freely + gating at SYN is simpler
and equally safe (no socket opens until SYN passes the allowlist).

---

## 6. Transparent relay: the key insight, concretely

When a guest SYN passes the allowlist, the `TcpEngine` does **not** proxy at L7. It:

1. Completes the TCP handshake **with the guest** locally (engine sends SYN-ACK as the
   destination IP). The guest now has an established TCP connection to "the mirror".
2. Opens a **fresh host `TcpStream`** to the real destination (the resolved IP, original
   port — 443 for HTTPS, 80 for HTTP).
3. Shuttles **raw payload bytes** both ways: guest-side TCP payload → host socket
   `write`; host socket `read` → guest-side TCP payload (re-segmented, re-windowed by the
   engine). This is the same byte-pump as `proxy/src/main.rs::handle`'s
   `ws_to_tcp` / `tcp_to_ws` join, minus the WebSocket framing — here the "client side"
   is the engine's guest-facing TCP instead of a WebSocket.

Because we relay **payload bytes** and never parse them, TLS is end-to-end: the guest's
TLS stack talks to the mirror's TLS stack; we are a dumb pipe. No certs, no MITM, no
trust changes in the guest. The allowlist is the only policy, applied at connect time.

FIN/RST mapping: guest FIN → `tcp_wr.shutdown()` on the host stream; host EOF (`read==0`)
→ engine sends FIN to the guest; host connect error → engine sends RST to the guest
(same path as an allowlist denial). Each direction's flow control is the guest's own TCP
window vs. the host socket's blocking/backpressure — the engine bridges the two windows.

---

## 7. Keeping it testable (frame-in / frame-out, like lan.rs)

The whole point of relocating `VirtualGateway` into an engine is to keep the engine
**unit-testable the same way lan.rs is** — hand-built frames in, expected frames out —
*without real sockets*. The seam is a trait:

```rust
/// Everything the TCP engine needs from the host network. Real impl uses
/// std/tokio sockets; tests use an in-memory fake.
pub trait HostNet: 'static {
    type Conn: HostConn;
    /// Open an upstream connection. In the fake this returns a pre-scripted
    /// Conn; the real impl does TcpStream::connect (allowlist already passed).
    fn connect(&mut self, ip: [u8; 4], port: u16) -> std::io::Result<Self::Conn>;
    /// Resolve a name to A-records. Fake returns a fixed map.
    fn resolve(&mut self, host: &str) -> std::io::Result<Vec<[u8; 4]>>;
}

pub trait HostConn {
    fn try_write(&mut self, buf: &[u8]) -> std::io::Result<usize>;
    fn try_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>; // 0 = EOF
    fn close(&mut self);
}
```

- **Engine tests** (`crates/vm/src/net/engine.rs #[cfg(test)]`): build a
  `NetEngine<FakeNet>` where `FakeNet` records writes and replays scripted reads. Feed it
  a guest SYN to `allowed.example:443`, assert it returns a SYN-ACK frame with correct
  checksums (reusing lan.rs's `ip_checksum` test style); feed payload, assert the
  `FakeConn` received those exact bytes; script a `FakeConn` read, call `poll_host`,
  assert the returned frame carries that payload to the guest. Feed a SYN to a
  **denied** host, assert the returned frame is a RST and `FakeNet::connect` was never
  called — pinning that **deny-by-default blocks the socket open itself**, not just the
  data.
- **Allowlist tests** already exist in `proxy/src/main.rs` and move with the shared type.
- **lan.rs tests** are untouched (ARP/ICMP/checksum vectors stay green).
- The **threading layer** (`net/thread.rs`) carries no protocol logic, so it needs only a
  thin smoke test: spawn it with a `FakeNet`-backed engine, push a `FromVm::GuestFrame`
  ARP request through the real `std::sync::mpsc`, assert a `ToVm::GuestFrame` ARP reply
  comes back. This exercises the channel handshake without sockets or a live guest.

This mirrors the project's stated pattern: pin the protocol/checksum maths in pure
frame-in/frame-out unit tests *before* it has to satisfy a real kernel, and let the
end-to-end Alpine run be the integration teeth.

---

## 8. Module layout (new code, all under `crates/vm/src/net/`)

```
crates/vm/src/net/
  mod.rs       // FromVm / ToVm enums, NetConfig, the public spawn() entry, NetHandle
  thread.rs    // net thread main loop (tokio current-thread runtime + mpsc bridge)
  engine.rs    // NetEngine<S: HostNet>: on_guest_frame / poll_host  (UNIT TESTED)
  dns.rs       // DnsResponder: answers guest DNS, records ip->host for the allowlist
  tcp.rs       // TcpEngine<S>: guest TCP <-> HostConn NAT + relay (smoltcp OR hand-rolled)
  host.rs      // HostNet/HostConn traits + the real std/tokio impl
  fake.rs      // #[cfg(test)] FakeNet/FakeConn for engine.rs tests
crates/vm/src/lan.rs   // UNCHANGED — VirtualGateway, now instantiated inside NetEngine
```

Shared policy (lifted out of the proxy binary, reused by both):
```
crates/net-policy/ (or pub from a shared spot)
  Allowlist, AllowEntry, from_env(), permits()  // + the existing deny-by-default tests
```

`NetHandle` is the small struct the VM loop holds:
```rust
pub struct NetHandle {
    pub to_net: std::sync::mpsc::Sender<FromVm>,
    pub from_net: std::sync::mpsc::Receiver<ToVm>,
    reoffer: std::collections::VecDeque<Vec<u8>>, // bounded RX re-offer buffer
}
impl NetHandle {
    pub fn pump(&mut self, vm: &mut Vm, loop_dt_ms: u32); // the §2 handshake, one call
}
```

The example's loop then shrinks to: `run_steps_idle_aware`; `net.pump(&mut vm, dt)`;
`drain_output`; feed stdin — networking is one `pump` call, fully non-blocking, and the
console stays live while sockets do their thing on the other thread.

---

## 9. Summary of decisions

| Question | Decision |
|---|---|
| Threading | VM stays single-threaded & non-`Send`; one dedicated `wwwvm-net` thread owns sockets. |
| Runtime on net thread | tokio current-thread (reuses proxy idioms); blocking-thread variant is acceptable. |
| Channels | `std::sync::mpsc`: `Sender<FromVm>` (VM→net, unbounded, never blocks) + `Receiver<ToVm>` (net→VM, drained via `try_iter`). |
| VM never blocks | `send` (unbounded) + `try_iter` (non-blocking) + bounded reoffer buffer for `inject_rx_frame == false`. |
| VirtualGateway | Moves into `NetEngine` on the net thread (single frame classifier); `lan.rs` code unchanged. |
| Allowlist | Reuse `proxy` `Allowlist`, lifted to a shared module; deny-by-default; `*` never default. |
| Allowlist check point | At guest TCP SYN, keyed on the hostname recorded by `DnsResponder` (IP fallback for literal-IP connects). |
| Denial signal | TCP **RST** to the guest (clean "refused", no socket opened) + stderr `Note`. |
| TLS | Transparent payload-byte relay; no decrypt/MITM; end-to-end guest↔mirror. |
| Testability | `HostNet`/`HostConn` traits → in-memory fake; engine unit-tested frame-in/frame-out like `lan.rs`. |
```
