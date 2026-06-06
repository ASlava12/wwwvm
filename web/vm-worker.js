// VM Web Worker — runs WwwVm off the UI thread.
//
// Why: the VM steps millions of instructions per frame; on the main thread a
// slow boot freezes the page and a backgrounded tab throttles/pauses
// requestAnimationFrame so the guest stalls. In a worker the loop is driven by
// setTimeout (not paused like rAF in background tabs) and never blocks the UI.
//
// This worker owns the whole VM lifecycle plus the network relay (WebSockets +
// DoH run fine in a worker) and framebuffer extraction. It talks to main.js
// over postMessage; see web/main.js for the client. The main thread keeps an
// identical inline implementation as a fallback (the "Web Worker" checkbox).
import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
import { classifyFrame } from "./net-route.js?v=1";

const ready = init(); // wasm init promise; every handler awaits it
let vm = null;
let idleAware = false;
let stepBudget = 50_000;
let running = false;
let tick = 0;

// ---- network relay (mirrors the main-thread fallback in main.js) ----
let netEnabled = false;
let switchNet = false; // virtual-LAN mode: raw L2 frames ↔ the in-page hub
let hybridNet = false; // LAN + NAT: peer frames → hub, gateway frames → NAT
let netProxyUrl = "ws://localhost:8080";
// LAN live-stats counters (reported to the lab UI): frames/bytes in & out of
// this VM's NIC, plus a boot timestamp for uptime.
let bootMs = 0;
let txFrames = 0;
let rxFrames = 0;
let txBytes = 0;
let rxBytes = 0;
// Upstream-proxy selection from main.js, merged into every connect frame:
// {} = direct, {auto:true} = server rotates, {upstream:{kind,host,port}} = chain.
let netUpstream = {};
const netConns = new Map();
// Warn once per boot if the relay can't be reached — otherwise networking just
// hangs silently (apk/wget never connect) with no clue the proxy isn't up.
let relayWarned = false;

// Resolve one host (DoH via Cloudflare) and feed the answer into the NAT's DNS
// cache. Deduped while in flight so the guest's DNS retries don't spam DoH.
const dnsInFlight = new Set();
async function resolveHost(host) {
  if (!host || dnsInFlight.has(host)) return;
  if (/^\d+\.\d+\.\d+\.\d+$/.test(host)) {
    vm.net_cache_dns(host, Uint8Array.from(host.split(".").map(Number)));
    return;
  }
  dnsInFlight.add(host);
  try {
    const r = await fetch(
      `https://cloudflare-dns.com/dns-query?name=${encodeURIComponent(host)}&type=A`,
      { headers: { accept: "application/dns-json" } });
    const j = await r.json();
    const ips = (j.Answer || []).filter((a) => a.type === 1).map((a) => a.data);
    const packed = new Uint8Array(ips.length * 4);
    ips.forEach((ip, i) => ip.split(".").forEach((o, k) => (packed[i * 4 + k] = +o)));
    const kept = vm.net_cache_dns(host, packed);
    post({ t: "net", text: `resolved ${host} → ${ips.join(", ") || "(none)"} (${kept} cached)` });
  } catch (e) {
    post({ t: "net", text: `DoH resolve failed for ${host}: ${e.message || e}` });
  } finally {
    dnsInFlight.delete(host);
  }
}

// Pre-resolve the allowlist's named hosts (skip "*" / empty). With an allow-all
// list this does nothing — names get resolved on demand from net_pump instead.
async function netPreResolve(allowlist) {
  const hosts = [
    ...new Set(allowlist.split(/[,\n]/).map((s) => s.trim().split(":")[0]).filter(Boolean)),
  ];
  for (const host of hosts) {
    if (host !== "*") resolveHost(host);
  }
}

function netOpenConn({ id, host, port }) {
  const c = { ws: null, open: false, hostClosed: false, pendingIn: [] };
  netConns.set(id, c);
  // An https page can't open a ws:// relay — the browser blocks it as mixed
  // content (the connection just fails). Call out that specific cause once,
  // before the generic "relay unreachable" hint.
  if (!relayWarned && self.location?.protocol === "https:" && /^ws:\/\//i.test(netProxyUrl)) {
    relayWarned = true;
    post({
      t: "output",
      text: `\r\n[net] this page is https but the relay is ws:// (${netProxyUrl}) — browsers ` +
        `block that as mixed content. Use a wss:// (TLS) relay.\r\n`,
    });
  }
  let ws;
  try {
    ws = new WebSocket(netProxyUrl);
  } catch (e) {
    if (!relayWarned) {
      relayWarned = true;
      post({ t: "output", text: `\r\n[net] bad relay URL "${netProxyUrl}": ${e.message || e}\r\n` });
    }
    vm.net_conn_closed(id);
    netConns.delete(id);
    return;
  }
  ws.binaryType = "arraybuffer";
  c.ws = ws;
  ws.onopen = () => {
    c.open = true;
    ws.send(JSON.stringify({ host, port, ...netUpstream }));
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") {
      c.hostClosed = true;
      try { ws.close(); } catch {}
      return;
    }
    c.pendingIn.push(new Uint8Array(ev.data));
  };
  ws.onclose = ws.onerror = () => {
    // Never opened → the relay is unreachable (not started / wrong URL / blocked).
    // Surface it once so a hung `apk`/`wget` has a visible cause.
    if (!c.open && !relayWarned) {
      relayWarned = true;
      post({
        t: "output",
        text: `\r\n[net] relay unreachable at ${netProxyUrl} — start it (cargo run -p wwwvm-proxy) ` +
          `and ensure its allowlist + WWWVM_PROXY_ORIGINS permit this page.\r\n`,
      });
    }
    c.hostClosed = true;
  };
}

function pumpNet() {
  if (!netEnabled) return;
  vm.net_pump(performance.now());
  pumpRelay();
}

// DNS + per-connection WebSocket plumbing shared by NAT and hybrid modes. Reads
// the NAT's pending work (new flows, DNS queries, write-closes) and shuttles
// payload over the proxy WebSockets. Assumes the NAT was already advanced
// (net_pump for pure-NAT, net_poll for hybrid).
function pumpRelay() {
  // On-demand DNS: resolve names the guest queried that weren't pre-cached
  // (the path that makes an allow-all "*" list work).
  let dnsReqs;
  try { dnsReqs = JSON.parse(vm.net_take_dns_requests()); } catch { dnsReqs = []; }
  for (const name of dnsReqs) resolveHost(name);
  let news;
  try { news = JSON.parse(vm.net_take_new_connections()); } catch { news = []; }
  for (const conn of news) netOpenConn(conn);
  for (const [id, c] of netConns) {
    while (c.pendingIn.length) {
      if (vm.net_conn_send(id, c.pendingIn[0])) c.pendingIn.shift();
      else break;
    }
    if (c.hostClosed) {
      if (c.pendingIn.length === 0) { vm.net_conn_closed(id); netConns.delete(id); }
      continue;
    }
    if (!c.open || c.ws.readyState !== WebSocket.OPEN) continue;
    const out = vm.net_conn_outbound(id);
    if (out === undefined) {
      try { c.ws.close(); } catch {}
      vm.net_conn_closed(id);
      netConns.delete(id);
      continue;
    }
    if (out.length) c.ws.send(out);
  }
  // Propagate guest write half-closes: a "FIN" control frame tells the proxy to
  // shut down the upstream write side without closing the WebSocket, so the
  // host→guest response keeps flowing. Reported once per connection.
  for (const id of vm.net_take_write_closed()) {
    const c = netConns.get(id);
    if (c && c.open && c.ws.readyState === WebSocket.OPEN) {
      try { c.ws.send("FIN"); } catch {}
    }
  }
}

function clearNet() {
  for (const c of netConns.values()) {
    if (c.ws) {
      c.ws.onopen = c.ws.onmessage = c.ws.onclose = c.ws.onerror = null;
      try { c.ws.close(); } catch {}
    }
  }
  netConns.clear();
}

// ---- posting state to main ----
function post(msg, transfer) { self.postMessage(msg, transfer || []); }

function postFb() {
  if (!vm.has_framebuffer || !vm.has_framebuffer()) return;
  const w = vm.framebuffer_width();
  const h = vm.framebuffer_height();
  const stride = vm.framebuffer_stride();
  if (!w || !h) return;
  const bytes = vm.framebuffer_bytes(); // fresh copy from wasm — safe to transfer
  if (bytes.length < stride * h) return;
  post({ t: "fb", w, h, stride, buf: bytes.buffer }, [bytes.buffer]);
}

const hex = (v, w = 8) => (v >>> 0).toString(16).padStart(w, "0").toUpperCase();
function postStatus() {
  if (!vm) return;
  post({
    t: "status",
    booted: vm.is_booted(),
    halted: vm.is_halted(),
    eip: hex(vm.get_eip()),
    eax: hex(vm.read_register_u32(0)),
    eflags: hex(vm.get_eflags(), 4),
    cr0: hex(vm.read_control_register(0)),
    cr3: hex(vm.read_control_register(3)),
    tsc: hex(vm.get_tsc_low()),
    lapic: hex(vm.get_lapic_current_count()),
    hpet: hex(vm.get_hpet_counter_low()),
    vga: vm.vga_text_snapshot(),
    net: netEnabled ? `live — ${vm.net_conn_count()} flow(s), ${netConns.size} socket(s)` : null,
  });
}

// Virtual-LAN mode: drain the frames the guest transmitted and hand them to the
// main-thread hub (which routes them through the L2 switch to peer VMs). Inbound
// frames arrive as "rx" messages → inject_rx_frame.
function pumpSwitch() {
  if (!vm) return;
  const frames = [];
  for (;;) {
    const f = vm.drain_tx_frame();
    if (f === undefined || f === null) break;
    txFrames++; txBytes += f.length;
    frames.push(f);
  }
  if (frames.length) post({ t: "tx", frames }, frames.map((f) => f.buffer));
}

// Hybrid LAN + NAT: one NIC serves both peer traffic and the outside world.
// Route each transmitted frame by destination (classifyFrame, the shared pure
// decision): "nat" → the NAT only; "both" → NAT (it answers gateway ARP/DHCP)
// AND the hub (peers see the broadcast); "switch" → peer unicast → the hub. The
// NAT's replies (net_pop_egress) are injected straight back into this VM's NIC.
function pumpHybrid() {
  if (!vm) return;
  const txToHub = [];
  for (;;) {
    const f = vm.drain_tx_frame();
    if (f === undefined || f === null) break;
    txFrames++; txBytes += f.length;
    const route = classifyFrame(f);
    if (route !== "switch") vm.net_push_frame(f); // "nat" or "both"
    if (route !== "nat") txToHub.push(f);         // "switch" or "both"
  }
  if (txToHub.length) post({ t: "tx", frames: txToHub }, txToHub.map((f) => f.buffer));
  // Advance the NAT and drain its replies into the NIC (bounded; requeue on a
  // full RX ring so ordering holds and we retry next tick).
  vm.net_poll(performance.now());
  let guard = 1024;
  while (guard-- > 0) {
    const f = vm.net_pop_egress();
    if (f === undefined || f === null) break;
    if (!vm.inject_rx_frame(f)) { vm.net_push_egress_front(f); break; }
    rxFrames++; rxBytes += f.length;
  }
  pumpRelay();
}

// Per-VM live stats for the LAN lab's right-hand list (uptime + RX/TX).
function postLanStat() {
  if (!vm || !bootMs) return;
  post({
    t: "stat",
    upMs: performance.now() - bootMs,
    txFrames, rxFrames, txBytes, rxBytes,
    flows: netEnabled ? vm.net_conn_count() : 0,
  });
}

function loop() {
  if (!running || !vm) return;
  // Idle (HLT) → small budget so we don't spin-burn a core; active → full budget.
  const idleNow = idleAware && vm.is_halted();
  const budget = idleNow ? 250_000 : stepBudget;
  let steps;
  try {
    steps = idleAware ? vm.run_idle_aware(budget) : vm.run(budget);
  } catch (e) {
    post({ t: "error", message: String((e && e.message) || e) });
    running = false;
    return;
  }
  void steps;
  const out = vm.read_output();
  if (out) post({ t: "output", text: out });
  if (vm.last_error) {
    post({ t: "error", message: vm.last_error });
    running = false;
    return;
  }
  if (hybridNet) pumpHybrid();
  else if (switchNet) pumpSwitch();
  else pumpNet();
  tick++;
  if (tick % 6 === 0) postFb();
  if (tick % 12 === 0) postStatus();
  if ((switchNet || hybridNet) && tick % 20 === 0) postLanStat();
  // A built-in demo's HLT is terminal; a booted Linux guest idles in HLT
  // (waiting for input/timer IRQs) so keep looping in idle-aware mode.
  if (vm.is_halted() && !idleAware) {
    running = false;
    postStatus();
    return;
  }
  setTimeout(loop, 0);
}

function startLoop() {
  if (!running) {
    running = true;
    setTimeout(loop, 0);
  }
}

self.onmessage = async (e) => {
  await ready;
  const m = e.data;
  try {
    switch (m.t) {
      case "boot": {
        running = false;
        clearNet();
        netEnabled = false;
        switchNet = false;
        hybridNet = false;
        relayWarned = false;
        tick = 0;
        bootMs = performance.now();
        txFrames = rxFrames = txBytes = rxBytes = 0;
        if (m.linux) {
          const ramMiB = m.ramMiB && m.ramMiB >= 64 ? m.ramMiB : 256;
          vm = WwwVm.new_with_ram_size(ramMiB * 1024 * 1024);
          // Seed the RTC with the host's real time (UTC — the guest treats the
          // RTC as UTC; the local TZ is applied on shell-ready by main.js) so
          // the guest's `date` is correct, not the 2026-01-01 default.
          if (typeof vm.set_cmos_time === "function") {
            const d = new Date();
            vm.set_cmos_time(
              d.getUTCFullYear() % 100, d.getUTCMonth() + 1, d.getUTCDate(),
              d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds()
            );
          }
          const entry = vm.load_bzimage(new Uint8Array(m.kernel));
          vm.set_kernel_cmdline(m.cmdline);
          if (m.initrd) vm.set_ramdisk(new Uint8Array(m.initrd));
          if (m.fb) vm.enable_framebuffer(m.fb.w, m.fb.h);
          if (m.net && m.net.mode === "switch") {
            // Virtual-LAN mode: no NAT. Give this VM its own MAC (so LAN peers
            // are distinct), then raw L2 frames flow to/from the in-page hub.
            switchNet = true;
            if (m.net.mac) vm.set_nic_mac(Uint8Array.from(m.net.mac));
          } else if (m.net && m.net.mode === "lan+nat") {
            // Hybrid: on the L2 switch for peers AND behind the in-wasm NAT for
            // the outside world. Distinct MAC + IP per VM; the NAT addresses its
            // replies to this VM's IP. pumpHybrid routes frames per-destination.
            hybridNet = true;
            netEnabled = true;
            netProxyUrl = m.net.proxyUrl;
            netUpstream = m.net.upstream || {};
            if (m.net.mac) vm.set_nic_mac(Uint8Array.from(m.net.mac));
            vm.net_enable_ip(m.net.allow, Uint8Array.from(m.net.ip || [10, 0, 2, 15]));
            netPreResolve(m.net.allow); // async; cache fills before the guest queries
          } else if (m.net) {
            netEnabled = true;
            netProxyUrl = m.net.proxyUrl;
            netUpstream = m.net.upstream || {};
            vm.net_enable(m.net.allow);
            netPreResolve(m.net.allow); // async; cache fills before the guest queries
          }
          vm.start_protected_mode_at(entry);
          idleAware = true;
          stepBudget = 1_500_000;
        } else {
          vm = new WwwVm();
          const b = m.builtin || {};
          if (b.kind === "interactive") vm.load_interactive_demo();
          else if (b.kind === "calculator") vm.load_calculator_demo();
          else if (b.kind === "pm") vm.load_pm_demo();
          else vm.load_default_guest();
          if (b.kind !== "pm") {
            vm.set_autorun(b.autorun || []);
            vm.boot();
          }
          idleAware = false;
          stepBudget = 50_000;
        }
        post({ t: "booted" });
        startLoop();
        break;
      }
      case "input":
        if (vm && vm.is_booted()) vm.send_input(new Uint8Array(m.bytes));
        break;
      case "command":
        if (vm) vm.send_command(m.text);
        break;
      case "scancodes":
        // PS/2 keyboard bytes (Set-1) for a graphical guest's 8042/evdev path.
        if (vm && vm.is_booted()) for (const c of m.codes) vm.push_scancode(c);
        break;
      case "mouse":
        // PS/2 mouse packet (dx/dy in PS/2 convention; buttons bitmask).
        if (vm && vm.is_booted()) vm.push_mouse_packet(m.dx | 0, m.dy | 0, m.buttons | 0);
        break;
      case "snapshot":
        if (vm && vm.is_booted()) {
          const b = vm.snapshot();
          post({ t: "snapshot", buf: b.buffer }, [b.buffer]);
        } else {
          post({ t: "snapshot", buf: null });
        }
        break;
      case "restore":
        if (vm) {
          vm.restore(new Uint8Array(m.buf));
          startLoop();
        }
        break;
      case "snapshot_export":
        if (vm && vm.is_booted()) {
          const b = vm.snapshot_export();
          post({ t: "snapshot_export", buf: b.buffer }, [b.buffer]);
        } else {
          post({ t: "snapshot_export", buf: null });
        }
        break;
      case "restore_export":
        if (vm) {
          vm.restore_export(new Uint8Array(m.buf));
          startLoop();
        }
        break;
      case "rx":
        // A frame from a LAN peer (via the hub) → deliver to this VM's NIC.
        if (vm && vm.is_booted()) {
          const frame = new Uint8Array(m.frame);
          if (vm.inject_rx_frame(frame)) { rxFrames++; rxBytes += frame.length; }
        }
        break;
    }
  } catch (err) {
    post({ t: "error", message: String((err && err.message) || err) });
  }
};
