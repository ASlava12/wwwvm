// Demo wiring: WASM `WwwVm` ↔ xterm.js + control panel.
//
// Build the wasm module first (see README): wasm-pack writes the bundle
// into ./pkg/ next to this file. The page works as plain static files
// served over http (any http server — Python's http.server is fine).

import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
import { saveSnapshot, loadSnapshot, listSnapshots } from "./storage.js";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const setStatus = (text, cls) => {
  statusEl.textContent = text;
  statusEl.className = "status" + (cls ? ` ${cls}` : "");
};

const term = new Terminal({
  fontFamily: "ui-monospace, monospace",
  fontSize: 13,
  theme: { background: "#000000", foreground: "#c9d1d9" },
  cursorBlink: true,
  convertEol: true,
});
term.open($("terminal"));
term.writeln("\x1b[90m(boot the VM to start)\x1b[0m");

await init();
// `vm` is reassignable: the Linux/Alpine boot path swaps in a fresh
// larger-RAM instance. Closures below capture this module-scoped binding,
// so they always see the current VM.
let vm = new WwwVm();
window.__wwwvm = vm;

// Forward terminal keystrokes to the guest UART.
term.onData((data) => {
  if (!vm.is_booted()) return;
  const bytes = new TextEncoder().encode(data);
  vm.send_input(bytes);
});

let rafHandle = 0;
const outputListeners = new Set();
// Per-frame step budget + stepping mode. The tiny built-in guests use a
// small polling budget with the HLT-terminal `run`; booting a real kernel
// (Linux/Alpine) needs the idle-aware stepper (Linux idles on HLT all
// through boot) and a much larger budget so it makes progress per frame.
let stepBudget = 50_000;
let idleAware = false;
const hex = (v, w = 8) => v.toString(16).padStart(w, "0").toUpperCase();

// Framebuffer → canvas blitter. The guest's efifb draws 32bpp pixels
// (little-endian B,G,R,X) into a reserved region of guest RAM; we copy
// them out and swap to the canvas's R,G,B,A byte order. Throttled —
// the console repaints rarely and the copy is ~2 MiB — and only after
// the kernel has actually brought up the framebuffer.
let fbFrame = 0;
const FB_EVERY = 6; // blit roughly every 6th animation frame
let fbImage = null;
function blitFramebuffer() {
  if (!vm.has_framebuffer || !vm.has_framebuffer()) return;
  const w = vm.framebuffer_width();
  const h = vm.framebuffer_height();
  const stride = vm.framebuffer_stride();
  if (!w || !h) return;
  const bytes = vm.framebuffer_bytes();
  if (bytes.length < stride * h) return;
  const cv = $("fb");
  if (cv.width !== w || cv.height !== h) {
    cv.width = w;
    cv.height = h;
    fbImage = null;
  }
  const ctx = cv.getContext("2d");
  if (!fbImage || fbImage.width !== w || fbImage.height !== h) {
    fbImage = ctx.createImageData(w, h);
  }
  const out = fbImage.data;
  for (let y = 0; y < h; y++) {
    let si = y * stride;
    let di = y * w * 4;
    for (let x = 0; x < w; x++) {
      out[di] = bytes[si + 2]; // R
      out[di + 1] = bytes[si + 1]; // G
      out[di + 2] = bytes[si]; // B
      out[di + 3] = 255; // A (ignore X)
      si += 4;
      di += 4;
    }
  }
  ctx.putImageData(fbImage, 0, 0);
  const fbStatus = $("fb-status");
  if (fbStatus) fbStatus.textContent = `${w}×${h}×32 efifb — live`;
}

// --- Networking relay ---
//
// The wasm side runs the smoltcp TCP NAT (same crate as native). It hands us
// per-connection byte queues; we tunnel each over a WebSocket to crates/proxy
// (WebSocket↔TCP, deny-by-default allowlist). DNS the guest asks for is
// pre-resolved here via DNS-over-HTTPS and pushed into the NAT's cache, since
// the browser can't do raw DNS. SECURITY: never run the proxy with a "*"
// allowlist on a reachable host — it's an open relay.
let netEnabled = false;
let netProxyUrl = "ws://localhost:8080";
const netConns = new Map(); // id -> { ws, open, pendingOut: [Uint8Array], pendingIn: [Uint8Array] }
const setNetStatus = (t) => { const el = $("net-status"); if (el) el.textContent = t; };

// Resolve each allowlisted host via Cloudflare DoH and seed the NAT's cache.
async function netPreResolve(allowlist) {
  const hosts = [...new Set(allowlist.split(",")
    .map((s) => s.trim().split(":")[0]).filter(Boolean))];
  for (const host of hosts) {
    // A bare IPv4 literal needs no lookup.
    if (/^\d+\.\d+\.\d+\.\d+$/.test(host)) {
      const ip = Uint8Array.from(host.split(".").map(Number));
      vm.net_cache_dns(host, ip);
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
      setNetStatus(`resolved ${host} → ${ips.join(", ") || "(none)"} (${kept} cached)`);
    } catch (e) {
      setNetStatus(`DoH resolve failed for ${host}: ${e.message || e}`);
    }
  }
}

function netOpenConn({ id, host, port }) {
  // hostClosed: the WebSocket ended — stop reading, but keep the slot until
  // every buffered inbound chunk has been handed to the guest (else the tail
  // of a response is dropped and the guest is FIN'd early).
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
    ws.send(JSON.stringify({ host, port })); // proxy handshake header
  };
  ws.onmessage = (ev) => {
    // The proxy sends TCP payload as BINARY frames; a TEXT frame is a control
    // message (e.g. "ERR …") — never inject it into the guest's byte stream
    // (that would corrupt TLS / the response). Fail the connection cleanly.
    if (typeof ev.data === "string") {
      console.warn("wwwvm proxy:", ev.data);
      c.hostClosed = true;
      try { ws.close(); } catch {}
      return;
    }
    c.pendingIn.push(new Uint8Array(ev.data)); // delivered to the guest in pumpNet()
  };
  ws.onclose = ws.onerror = () => {
    c.hostClosed = true; // drained + torn down in pumpNet once pendingIn empties
  };
}

function pumpNet() {
  if (!netEnabled) return;
  vm.net_pump(performance.now());

  // New flows → open a WebSocket each.
  let news;
  try { news = JSON.parse(vm.net_take_new_connections()); } catch { news = []; }
  for (const conn of news) netOpenConn(conn);

  // Shuttle bytes for each live connection.
  for (const [id, c] of netConns) {
    // host → guest: flush what the WebSocket delivered (respect backpressure).
    while (c.pendingIn.length) {
      if (vm.net_conn_send(id, c.pendingIn[0])) c.pendingIn.shift();
      else break; // NAT queue full — retry next tick
    }

    // WebSocket closed/errored: finish the teardown only once every buffered
    // inbound byte has been accepted by the guest, so the response tail isn't
    // truncated and the guest's FIN comes after the data.
    if (c.hostClosed) {
      if (c.pendingIn.length === 0) {
        vm.net_conn_closed(id);
        netConns.delete(id);
      }
      continue;
    }

    // guest → host: drain the NAT only once the WebSocket is OPEN — before
    // that, unflushed bytes stay queued in the NAT (real backpressure) rather
    // than a JS array that could be discarded if the flow dies first. This is
    // also where we learn the NAT reaped the flow (outbound === undefined).
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
  setNetStatus(`live — ${vm.net_conn_count()} flow(s), ${netConns.size} socket(s)`);
}
function pump() {
  const steps = idleAware ? vm.run_idle_aware(stepBudget) : vm.run(stepBudget);
  const out = vm.read_output();
  if (out) {
    term.write(out);
    for (const l of outputListeners) l(out);
  }
  if (vm.last_error) {
    setStatus(`cpu error: ${vm.last_error}`, "error");
    $("diag").textContent = vm.last_error;
    return;
  }
  // First line: high-level status. Second line: register dump that's
  // useful when watching a PM kernel — CR0.PE/PG, CR3 (page-dir base),
  // EIP (PC), EAX (often a return/exit value), low 16 of EFLAGS, TSC
  // low half so users can see the VM ticking. Third line: timer state
  // — LAPIC current count and HPET main counter (low 32).
  const eip = hex(vm.get_eip());
  const eax = hex(vm.read_register_u32(0));
  const flags = hex(vm.get_eflags(), 4);
  const cr0 = hex(vm.read_control_register(0));
  const cr3 = hex(vm.read_control_register(3));
  const tsc = hex(vm.get_tsc_low());
  const lapic = hex(vm.get_lapic_current_count());
  const hpet = hex(vm.get_hpet_counter_low());
  $("diag").textContent =
    `booted=${vm.is_booted()}  halted=${vm.is_halted()}  steps/frame=${steps}\n` +
    `EIP=${eip}  EAX=${eax}  EFLAGS=${flags}  CR0=${cr0}  CR3=${cr3}  TSC=${tsc}\n` +
    `LAPIC_CURR=${lapic}  HPET=${hpet}`;
  // VGA snapshot — only render when the buffer has something visible
  // so the pane doesn't get flooded with 25 lines of blank for guests
  // that never touch 0xB8000.
  const vga = vm.vga_text_snapshot();
  if (vga.trim().length > 0) {
    $("vga").textContent = vga;
  }
  // Drive the network relay (no-op until enabled), then repaint the
  // framebuffer canvas every few frames (cheap throttle).
  pumpNet();
  if (++fbFrame % FB_EVERY === 0) blitFramebuffer();
  if (vm.is_halted()) {
    setStatus("halted", "");
    return;
  }
  rafHandle = requestAnimationFrame(pump);
}

$("boot").addEventListener("click", () => {
  if (rafHandle) cancelAnimationFrame(rafHandle);
  term.reset();
  // Built-in demos use the small HLT-terminal stepper.
  idleAware = false;
  stepBudget = 50_000;
  const autorun = $("autorun").value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  const guestKind = document.querySelector('input[name="guest"]:checked')?.value || "default";
  if (guestKind === "interactive") {
    vm.load_interactive_demo();
  } else if (guestKind === "calculator") {
    vm.load_calculator_demo();
  } else if (guestKind === "pm") {
    // PM-kernel demo doesn't go through reset_to_boot — load_pm_demo
    // calls start_protected_mode_at, which sets booted=true itself.
    // Skip vm.boot() so we don't undo it.
    try {
      vm.load_pm_demo();
    } catch (e) {
      setStatus(`load_pm_demo: ${e}`, "error");
      return;
    }
    setStatus("running", "running");
    pump();
    return;
  } else {
    vm.load_default_guest();
  }
  vm.set_autorun(autorun);
  vm.boot();
  setStatus("running", "running");
  pump();
});

// Boot a real Linux/Alpine kernel: fresh 256 MiB VM, load the bzImage +
// (optional) initramfs, hand off to protected mode, then pump with the
// idle-aware stepper. The guest's serial console (console=ttyS0) streams to
// the terminal. Boot is slow in wasm (a kernel is hundreds of millions of
// steps); the page stays usable but the frame budget makes it churn.
const setLinuxStatus = (text, cls = "") => {
  const el = $("linux-status");
  el.textContent = text;
  el.className = "status" + (cls ? ` ${cls}` : "");
};

$("boot-linux").addEventListener("click", async () => {
  const kfile = $("kernel-file").files?.[0];
  if (!kfile) {
    setLinuxStatus("pick a kernel (vmlinuz bzImage) first", "error");
    return;
  }
  if (rafHandle) cancelAnimationFrame(rafHandle);
  try {
    setLinuxStatus("reading kernel…");
    const kbytes = new Uint8Array(await kfile.arrayBuffer());
    const ifile = $("initrd-file").files?.[0];
    const ibytes = ifile ? new Uint8Array(await ifile.arrayBuffer()) : null;

    // Fresh VM with the headroom Alpine needs (the default demo VM is tiny).
    vm = WwwVm.new_with_ram_size(256 * 1024 * 1024);
    window.__wwwvm = vm;
    term.reset();

    const entry = vm.load_bzimage(kbytes);
    vm.set_kernel_cmdline($("cmdline").value);
    if (ibytes) vm.set_ramdisk(ibytes);
    // Host networking: spin up the in-wasm TCP NAT, pre-resolve the allowed
    // hosts over DoH, and relay each flow over a WebSocket to crates/proxy.
    // Tear down any sockets from a previous boot FIRST, with their handlers
    // detached, so a late onclose can't touch the fresh NAT's (reset) ids.
    for (const c of netConns.values()) {
      if (c.ws) {
        c.ws.onopen = c.ws.onmessage = c.ws.onclose = c.ws.onerror = null;
        try { c.ws.close(); } catch {}
      }
    }
    netConns.clear();
    netEnabled = $("net-enable").checked;
    if (netEnabled) {
      netProxyUrl = $("net-proxy").value.trim() || "ws://localhost:8080";
      const allow = $("net-allow").value.trim();
      vm.net_enable(allow);
      setNetStatus("enabled — resolving hosts…");
      netPreResolve(allow); // async; cache fills before the guest queries
    } else {
      setNetStatus("(off)");
    }
    // Advertise a linear framebuffer so the kernel's efifb binds and
    // fbcon renders the console as pixels (needs `console=tty0` on the
    // cmdline, which the default value includes). Enable BEFORE the PM
    // hand-off, which writes screen_info + reserves the e820 region.
    if ($("fb-enable").checked) {
      const [fbw, fbh] = ($("fb-res").value || "800x600").split("x").map(Number);
      vm.enable_framebuffer(fbw, fbh);
      const cv = $("fb");
      cv.width = fbw;
      cv.height = fbh;
      fbImage = null;
      $("fb-status").textContent = `${fbw}×${fbh}×32 efifb — waiting for kernel…`;
    }
    vm.start_protected_mode_at(entry); // sets booted=true itself

    idleAware = true;
    stepBudget = 1_500_000;
    const kib = (b) => `${(b >> 10).toLocaleString()} KiB`;
    setLinuxStatus(
      `booting — kernel ${kib(kbytes.length)}` +
        (ibytes ? `, initramfs ${kib(ibytes.length)}` : "") +
        " (slow in wasm; watch the terminal)",
      "running"
    );
    setStatus("running", "running");
    pump();
  } catch (e) {
    setLinuxStatus(`boot failed: ${e.message || e}`, "error");
    setStatus("idle", "");
  }
});

// `runCommand()` — send a line, collect output until it stops growing
// for ~250ms, return as a Promise. JS callers can await the result
// without writing their own pump loop. Uses a tap into pump()'s output
// stream so it does not steal data from the terminal.
async function runCommand(text, timeoutMs = 1500) {
  if (!vm.is_booted()) throw new Error("VM not booted");
  let collected = "";
  const listener = (chunk) => { collected += chunk; };
  outputListeners.add(listener);
  try {
    vm.send_command(text);
    const start = performance.now();
    let lastLen = -1;
    while (performance.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 80));
      if (collected.length === lastLen && collected.length > 0) break;
      lastLen = collected.length;
    }
  } finally {
    outputListeners.delete(listener);
  }
  return collected;
}
window.runCommand = runCommand;

$("send").addEventListener("click", async () => {
  const text = $("cmd").value;
  if (!text) return;
  try {
    const result = await runCommand(text);
    $("last-result").textContent = result || "(no output)";
  } catch (e) {
    $("last-result").textContent = String(e);
  }
});
$("cmd").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("send").click();
});

const setSnapshotStatus = (text, cls = "") => {
  const el = $("snapshot-status");
  el.textContent = text;
  el.className = "status" + (cls ? ` ${cls}` : "");
};

// Surface whether IndexedDB has anything stashed without forcing the
// user to click Load to find out.
(async () => {
  try {
    const keys = await listSnapshots();
    if (keys.length > 0) {
      setSnapshotStatus(`saved: ${keys.join(", ")}`);
    }
  } catch (e) {
    setSnapshotStatus(`IndexedDB unavailable: ${e.message || e}`, "error");
  }
})();

$("save").addEventListener("click", async () => {
  if (!vm.is_booted()) {
    setSnapshotStatus("can't snapshot before boot", "error");
    return;
  }
  try {
    const bytes = vm.snapshot();
    await saveSnapshot("latest", bytes);
    setSnapshotStatus(`saved ${bytes.length.toLocaleString()} bytes to IndexedDB`);
  } catch (e) {
    setSnapshotStatus(`save failed: ${e.message || e}`, "error");
  }
});

function resumePump() {
  if (rafHandle) cancelAnimationFrame(rafHandle);
  setStatus("running", "running");
  pump();
}

$("load").addEventListener("click", async () => {
  try {
    const bytes = await loadSnapshot("latest");
    if (!bytes) {
      setSnapshotStatus("no snapshot stored", "error");
      return;
    }
    vm.restore(bytes);
    term.reset();
    setSnapshotStatus(`restored ${bytes.length.toLocaleString()} bytes`);
    resumePump();
  } catch (e) {
    setSnapshotStatus(`load failed: ${e.message || e}`, "error");
  }
});

$("download").addEventListener("click", () => {
  if (!vm.is_booted()) {
    setSnapshotStatus("can't snapshot before boot", "error");
    return;
  }
  const bytes = vm.snapshot();
  const blob = new Blob([bytes], { type: "application/octet-stream" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  a.download = `wwwvm-${stamp}.bin`;
  a.click();
  URL.revokeObjectURL(url);
  setSnapshotStatus(`downloaded ${bytes.length.toLocaleString()} bytes`);
});

$("upload-trigger").addEventListener("click", () => $("upload").click());
$("upload").addEventListener("change", async (e) => {
  const file = e.target.files?.[0];
  if (!file) return;
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    vm.restore(bytes);
    term.reset();
    setSnapshotStatus(`uploaded + restored ${bytes.length.toLocaleString()} bytes`);
    resumePump();
  } catch (err) {
    setSnapshotStatus(`upload failed: ${err.message || err}`, "error");
  } finally {
    e.target.value = ""; // allow re-selecting the same file
  }
});
