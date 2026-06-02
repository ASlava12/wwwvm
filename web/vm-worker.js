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

const ready = init(); // wasm init promise; every handler awaits it
let vm = null;
let idleAware = false;
let stepBudget = 50_000;
let running = false;
let tick = 0;

// ---- network relay (mirrors the main-thread fallback in main.js) ----
let netEnabled = false;
let netProxyUrl = "ws://localhost:8080";
const netConns = new Map();

async function netPreResolve(allowlist) {
  const hosts = [
    ...new Set(allowlist.split(",").map((s) => s.trim().split(":")[0]).filter(Boolean)),
  ];
  for (const host of hosts) {
    if (/^\d+\.\d+\.\d+\.\d+$/.test(host)) {
      vm.net_cache_dns(host, Uint8Array.from(host.split(".").map(Number)));
      continue;
    }
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
    }
  }
}

function netOpenConn({ id, host, port }) {
  const c = { ws: null, open: false, hostClosed: false, pendingIn: [] };
  netConns.set(id, c);
  let ws;
  try {
    ws = new WebSocket(netProxyUrl);
  } catch (e) {
    vm.net_conn_closed(id);
    netConns.delete(id);
    return;
  }
  ws.binaryType = "arraybuffer";
  c.ws = ws;
  ws.onopen = () => {
    c.open = true;
    ws.send(JSON.stringify({ host, port }));
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") {
      c.hostClosed = true;
      try { ws.close(); } catch {}
      return;
    }
    c.pendingIn.push(new Uint8Array(ev.data));
  };
  ws.onclose = ws.onerror = () => { c.hostClosed = true; };
}

function pumpNet() {
  if (!netEnabled) return;
  vm.net_pump(performance.now());
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
  pumpNet();
  tick++;
  if (tick % 6 === 0) postFb();
  if (tick % 12 === 0) postStatus();
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
        tick = 0;
        if (m.linux) {
          vm = WwwVm.new_with_ram_size(256 * 1024 * 1024);
          const entry = vm.load_bzimage(new Uint8Array(m.kernel));
          vm.set_kernel_cmdline(m.cmdline);
          if (m.initrd) vm.set_ramdisk(new Uint8Array(m.initrd));
          if (m.fb) vm.enable_framebuffer(m.fb.w, m.fb.h);
          if (m.net) {
            netEnabled = true;
            netProxyUrl = m.net.proxyUrl;
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
    }
  } catch (err) {
    post({ t: "error", message: String((err && err.message) || err) });
  }
};
