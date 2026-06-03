// Demo wiring: WASM `WwwVm` ↔ xterm.js + control panel.
//
// Build the wasm module first (see README): wasm-pack writes the bundle
// into ./pkg/ next to this file. The page works as plain static files
// served over http (any http server — Python's http.server is fine).

import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
import { saveSnapshot, loadSnapshot, listSnapshots } from "./storage.js";
import { makeBytes, breakBytes } from "./ps2-keymap.js";

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
// so they always see the current VM. NOTE: `vm` is only used by the
// main-thread (inline) fallback; by default the VM runs in a Web Worker
// (see vm-worker.js) so a slow boot / backgrounded tab can't freeze the UI.
let vm = new WwwVm();
window.__wwwvm = vm;

// The VM engine: by default a Web Worker; the "Web Worker" checkbox (default
// on) can switch boots to the inline main-thread path. `active` tracks which
// engine currently owns the live VM, so input/snapshot route to the right one.
const worker = new Worker(new URL("./vm-worker.js", import.meta.url), { type: "module" });
let active = "inline"; // "inline" | "worker"
let useWorker = true;
let workerBooted = false;
let snapshotResolve = null;

function renderDiag(m) {
  $("diag").textContent =
    `booted=${m.booted}  halted=${m.halted}  (worker)\n` +
    `EIP=${m.eip}  EAX=${m.eax}  EFLAGS=${m.eflags}  CR0=${m.cr0}  CR3=${m.cr3}  TSC=${m.tsc}\n` +
    `LAPIC_CURR=${m.lapic}  HPET=${m.hpet}`;
}

worker.onmessage = (e) => {
  const m = e.data;
  switch (m.t) {
    case "booted":
      workerBooted = true;
      break;
    case "output":
      if (m.text) {
        term.write(m.text);
        for (const l of outputListeners) l(m.text);
      }
      break;
    case "status":
      workerBooted = m.booted;
      renderDiag(m);
      if (m.vga && m.vga.trim().length > 0) $("vga").textContent = m.vga;
      if (m.net != null) setNetStatus(m.net);
      setStatus(m.halted ? "idle — waiting for input" : "running", "running");
      break;
    case "fb":
      paintFb(m.w, m.h, m.stride, new Uint8Array(m.buf));
      break;
    case "net":
      setNetStatus(m.text);
      break;
    case "error":
      setStatus(`cpu error: ${m.message}`, "error");
      $("diag").textContent = m.message;
      break;
    case "snapshot":
      if (snapshotResolve) {
        snapshotResolve(m.buf ? new Uint8Array(m.buf) : null);
        snapshotResolve = null;
      }
      break;
  }
};

// Forward terminal keystrokes to the guest UART (worker or inline VM).
term.onData((data) => {
  const bytes = new TextEncoder().encode(data);
  if (active === "worker") {
    if (workerBooted) worker.postMessage({ t: "input", bytes }, [bytes.buffer]);
    return;
  }
  if (!vm.is_booted()) return;
  vm.send_input(bytes);
});

// PS/2 input → the framebuffer canvas. The terminal path above feeds the UART
// console; a graphical guest (Xorg) instead reads the 8042 keyboard + PS/2
// mouse via /dev/input/event*. Click the canvas to focus it, then keystrokes
// and pointer motion over it become Set-1 scan codes + mouse packets. Routes
// to whichever engine (worker / inline) owns the live VM.
function sendScancodes(codes) {
  if (!codes) return;
  if (active === "worker") {
    if (workerBooted) worker.postMessage({ t: "scancodes", codes });
    return;
  }
  if (vm.is_booted()) for (const c of codes) vm.push_scancode(c);
}
function sendMouse(dx, dy, buttons) {
  if (active === "worker") {
    if (workerBooted) worker.postMessage({ t: "mouse", dx, dy, buttons });
    return;
  }
  if (vm.is_booted()) vm.push_mouse_packet(dx, dy, buttons);
}

(function wireCanvasInput() {
  const cv = $("fb");
  if (!cv) return;
  cv.tabIndex = 0; // focusable, so it can receive key events
  let buttons = 0; // bit0 left, bit1 right, bit2 middle
  const BTN = { 0: 1, 1: 4, 2: 2 }; // DOM button (0/1/2) → our bitmask

  cv.addEventListener("keydown", (e) => {
    const b = makeBytes(e.code);
    if (b) { e.preventDefault(); sendScancodes(b); }
  });
  cv.addEventListener("keyup", (e) => {
    const b = breakBytes(e.code);
    if (b) { e.preventDefault(); sendScancodes(b); }
  });
  cv.addEventListener("mousemove", (e) => {
    const dx = e.movementX | 0;
    const dy = -(e.movementY | 0); // screen y grows down; PS/2 +y is up
    if (dx || dy) sendMouse(dx, dy, buttons);
  });
  cv.addEventListener("mousedown", (e) => {
    cv.focus();
    buttons |= BTN[e.button] || 0;
    sendMouse(0, 0, buttons);
    e.preventDefault();
  });
  cv.addEventListener("mouseup", (e) => {
    buttons &= ~(BTN[e.button] || 0);
    sendMouse(0, 0, buttons);
    e.preventDefault();
  });
  // Let right-click reach the guest instead of opening the page context menu.
  cv.addEventListener("contextmenu", (e) => e.preventDefault());
})();

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
// Paint a 32bpp B,G,R,X framebuffer onto the canvas (swap to R,G,B,A). Shared
// by the worker (which posts the pixel buffer) and the inline blit below.
function paintFb(w, h, stride, bytes) {
  if (!w || !h || bytes.length < stride * h) return;
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
function blitFramebuffer() {
  if (!vm.has_framebuffer || !vm.has_framebuffer()) return;
  const w = vm.framebuffer_width();
  const h = vm.framebuffer_height();
  const stride = vm.framebuffer_stride();
  if (!w || !h) return;
  paintFb(w, h, stride, vm.framebuffer_bytes());
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
  // Propagate guest write half-closes: a "FIN" control frame tells the proxy to
  // shut down the upstream write side without closing the WebSocket, so the
  // host→guest response keeps flowing. Reported once per connection.
  for (const id of vm.net_take_write_closed()) {
    const c = netConns.get(id);
    if (c && c.open && c.ws.readyState === WebSocket.OPEN) {
      try { c.ws.send("FIN"); } catch {}
    }
  }
  setNetStatus(`live — ${vm.net_conn_count()} flow(s), ${netConns.size} socket(s)`);
}
function pump() {
  // When the guest is idle (parked in HLT waiting for an IRQ — e.g. a
  // keystroke), a full step budget would just spin through HLT burning the
  // UI thread. Use a small budget then: enough to tick timers and pick up
  // injected input, cheap when nothing's happening. Active work (boot,
  // running a command) clears the halt, so the next frame uses the full
  // budget again.
  const idleNow = idleAware && vm.is_halted();
  const budget = idleNow ? 250_000 : stepBudget;
  const steps = idleAware ? vm.run_idle_aware(budget) : vm.run(budget);
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
    // A built-in demo's HLT is terminal — really done. But a booted Linux
    // guest parks in HLT whenever it's idle (waiting for a keystroke or the
    // next timer tick); that is NOT the end — keep pumping so injected input
    // and timer IRQs wake it. Otherwise the loop stops and the shell goes
    // dead to the keyboard.
    if (!idleAware) {
      setStatus("halted", "");
      return;
    }
    setStatus("idle — waiting for input", "running");
  }
  rafHandle = requestAnimationFrame(pump);
}

$("boot").addEventListener("click", () => {
  if (rafHandle) cancelAnimationFrame(rafHandle);
  term.reset();
  const autorun = $("autorun").value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  const guestKind = document.querySelector('input[name="guest"]:checked')?.value || "default";
  useWorker = $("worker-enable")?.checked ?? true;
  if (useWorker) {
    active = "worker";
    workerBooted = false;
    worker.postMessage({ t: "boot", linux: false, builtin: { kind: guestKind, autorun } });
    setStatus("running", "running");
    term.focus();
    return;
  }
  // ---- inline (main-thread) fallback ----
  active = "inline";
  // Built-in demos use the small HLT-terminal stepper.
  idleAware = false;
  stepBudget = 50_000;
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

// Boot a real Linux/Alpine kernel from in-memory buffers (kernel bzImage +
// optional initramfs cpio as ArrayBuffers). Shared by the server-image picker
// and the "load your own files" fallback. cmdline / framebuffer / networking
// are read from the control panel (the image picker fills those in first).
// `ramMiB` sizes the guest RAM — the GUI image asks for more (framebuffer +
// the DRM/input modules unpacked into the initramfs tmpfs).
async function bootLinux(kbuf, ibuf, { ramMiB = 256 } = {}) {
  if (rafHandle) cancelAnimationFrame(rafHandle);
  useWorker = $("worker-enable")?.checked ?? true;
  const cmdline = $("cmdline").value;
  const fbEnabled = $("fb-enable").checked;
  const [fbw, fbh] = ($("fb-res").value || "800x600").split("x").map(Number);
  const kib = (n) => `${(n >> 10).toLocaleString()} KiB`;

  // ---- Web Worker path (default): hand the VM off the UI thread ----
  if (useWorker) {
    try {
      const kLen = kbuf.byteLength;
      const iLen = ibuf ? ibuf.byteLength : 0;
      term.reset();
      active = "worker";
      workerBooted = false;

      let fb = null;
      if (fbEnabled) {
        const cv = $("fb");
        cv.width = fbw;
        cv.height = fbh;
        fbImage = null;
        $("fb-status").textContent = `${fbw}×${fbh}×32 efifb — waiting for kernel…`;
        fb = { w: fbw, h: fbh };
      }
      let net = null;
      if ($("net-enable").checked) {
        net = {
          proxyUrl: $("net-proxy").value.trim() || "ws://localhost:8080",
          allow: $("net-allow").value.trim(),
        };
        setNetStatus("enabled — resolving hosts…");
      } else {
        setNetStatus("(off)");
      }

      const transfer = [kbuf];
      if (ibuf) transfer.push(ibuf);
      worker.postMessage(
        { t: "boot", linux: true, kernel: kbuf, initrd: ibuf, cmdline, fb, net, ramMiB },
        transfer
      );
      setLinuxStatus(
        `booting in worker — kernel ${kib(kLen)}` +
          (iLen ? `, initramfs ${kib(iLen)}` : "") +
          ` (${ramMiB} MiB RAM, off the UI thread)`,
        "running"
      );
      setStatus("running", "running");
      term.focus();
    } catch (e) {
      setLinuxStatus(`boot failed: ${e.message || e}`, "error");
      setStatus("idle", "");
    }
    return;
  }

  // ---- inline (main-thread) fallback ----
  active = "inline";
  try {
    const kbytes = new Uint8Array(kbuf);
    const ibytes = ibuf ? new Uint8Array(ibuf) : null;

    // Fresh VM with the requested headroom (the default demo VM is tiny).
    vm = WwwVm.new_with_ram_size(ramMiB * 1024 * 1024);
    window.__wwwvm = vm;
    term.reset();

    const entry = vm.load_bzimage(kbytes);
    vm.set_kernel_cmdline(cmdline);
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
    // cmdline — the GUI image's cmdline includes it). Enable BEFORE the PM
    // hand-off, which writes screen_info + reserves the e820 region.
    if (fbEnabled) {
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
    setLinuxStatus(
      `booting — kernel ${kib(kbytes.length)}` +
        (ibytes ? `, initramfs ${kib(ibytes.length)}` : "") +
        ` (${ramMiB} MiB RAM; slow in wasm, watch the terminal)`,
      "running"
    );
    setStatus("running", "running");
    term.focus(); // so keystrokes reach the guest without an extra click
    pump();
  } catch (e) {
    setLinuxStatus(`boot failed: ${e.message || e}`, "error");
    setStatus("idle", "");
  }
}

// "Boot from files" fallback — read the file inputs, then boot (256 MiB).
$("boot-linux").addEventListener("click", async () => {
  const kfile = $("kernel-file").files?.[0];
  if (!kfile) {
    setLinuxStatus("pick a kernel (vmlinuz bzImage) first", "error");
    return;
  }
  setLinuxStatus("reading kernel…");
  const kbuf = await kfile.arrayBuffer();
  const ifile = $("initrd-file").files?.[0];
  const ibuf = ifile ? await ifile.arrayBuffer() : null;
  bootLinux(kbuf, ibuf, { ramMiB: 256 });
});

// ---- Server image picker -------------------------------------------------
// List the images the server advertises in images/manifest.json, fetch the
// chosen kernel + initramfs, and boot via bootLinux(). Picking an image fills
// the cmdline / framebuffer controls so the user sees (and can tweak) what it
// will boot with. Falls back gracefully when no images are built.
const IMAGES_BASE = "images/";
let imageManifest = [];

function applyImageToControls(img) {
  if (!img) return;
  if (img.cmdline) $("cmdline").value = img.cmdline;
  $("fb-enable").checked = !!img.gui;
  if (img.fbRes) {
    const sel = $("fb-res");
    if (![...sel.options].some((o) => o.value === img.fbRes)) {
      const o = document.createElement("option");
      o.value = img.fbRes;
      o.textContent = img.fbRes.replace("x", "×");
      sel.appendChild(o);
    }
    sel.value = img.fbRes;
  }
}

async function loadImageManifest() {
  const sel = $("image-select");
  try {
    const r = await fetch(IMAGES_BASE + "manifest.json", { cache: "no-cache" });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const j = await r.json();
    imageManifest = Array.isArray(j.images) ? j.images : [];
    if (!imageManifest.length) throw new Error("manifest lists no images");
    sel.innerHTML = "";
    for (const img of imageManifest) {
      const o = document.createElement("option");
      o.value = img.id;
      const mb = img.bytes ? ` (~${Math.round(img.bytes / (1 << 20))} MiB)` : "";
      o.textContent = (img.name || img.id) + mb;
      sel.appendChild(o);
    }
    $("boot-image").disabled = false;
    applyImageToControls(imageManifest[0]);
    $("image-status").textContent = `${imageManifest.length} image(s) — pick one and Load`;
  } catch (e) {
    sel.innerHTML = '<option value="">(no images)</option>';
    $("boot-image").disabled = true;
    $("image-status").textContent =
      `no server images — run scripts/build-web-images.sh (${e.message || e})`;
  }
}

$("image-select").addEventListener("change", () => {
  applyImageToControls(imageManifest.find((x) => x.id === $("image-select").value));
});

$("boot-image").addEventListener("click", async () => {
  const img = imageManifest.find((x) => x.id === $("image-select").value);
  if (!img) {
    $("image-status").textContent = "pick an image first";
    return;
  }
  applyImageToControls(img); // ensure cmdline / fb match the selection
  const btn = $("boot-image");
  btn.disabled = true;
  try {
    $("image-status").textContent = `fetching ${img.name}…`;
    setLinuxStatus(`downloading ${img.kernel} + ${img.initramfs}…`, "running");
    // Default cache: the server's Last-Modified lets a re-boot revalidate with
    // If-Modified-Since (304, no re-download) yet pick up a rebuilt image.
    const [kr, ir] = await Promise.all([
      fetch(IMAGES_BASE + img.kernel),
      fetch(IMAGES_BASE + img.initramfs),
    ]);
    if (!kr.ok) throw new Error(`kernel HTTP ${kr.status}`);
    if (!ir.ok) throw new Error(`initramfs HTTP ${ir.status}`);
    const kbuf = await kr.arrayBuffer();
    const ibuf = await ir.arrayBuffer();
    $("image-status").textContent = `booting ${img.name}`;
    await bootLinux(kbuf, ibuf, { ramMiB: img.ramMiB || 256 });
  } catch (e) {
    $("image-status").textContent = `image boot failed: ${e.message || e}`;
    setLinuxStatus(`image boot failed: ${e.message || e}`, "error");
  } finally {
    btn.disabled = false;
  }
});

loadImageManifest();

// `runCommand()` — send a line, collect output until it stops growing
// for ~250ms, return as a Promise. JS callers can await the result
// without writing their own pump loop. Uses a tap into pump()'s output
// stream so it does not steal data from the terminal.
async function runCommand(text, timeoutMs = 1500) {
  const booted = active === "worker" ? workerBooted : vm.is_booted();
  if (!booted) throw new Error("VM not booted");
  let collected = "";
  const listener = (chunk) => { collected += chunk; };
  outputListeners.add(listener);
  try {
    if (active === "worker") worker.postMessage({ t: "command", text });
    else vm.send_command(text);
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

// Snapshot/restore work against whichever engine owns the VM. In worker mode
// the bytes come back asynchronously over postMessage.
const isBooted = () => (active === "worker" ? workerBooted : vm.is_booted());
async function getSnapshot() {
  if (active === "worker") {
    return await new Promise((resolve) => {
      snapshotResolve = resolve;
      worker.postMessage({ t: "snapshot" });
    });
  }
  return vm.snapshot();
}
function restoreSnapshot(bytes) {
  if (active === "worker") {
    worker.postMessage({ t: "restore", buf: bytes.buffer }, [bytes.buffer]);
    term.reset();
  } else {
    vm.restore(bytes);
    term.reset();
    resumePump();
  }
}

$("save").addEventListener("click", async () => {
  if (!isBooted()) {
    setSnapshotStatus("can't snapshot before boot", "error");
    return;
  }
  try {
    const bytes = await getSnapshot();
    if (!bytes) throw new Error("no snapshot returned");
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
    restoreSnapshot(bytes);
    setSnapshotStatus(`restored ${bytes.length.toLocaleString()} bytes`);
  } catch (e) {
    setSnapshotStatus(`load failed: ${e.message || e}`, "error");
  }
});

$("download").addEventListener("click", async () => {
  if (!isBooted()) {
    setSnapshotStatus("can't snapshot before boot", "error");
    return;
  }
  const bytes = await getSnapshot();
  if (!bytes) {
    setSnapshotStatus("no snapshot returned", "error");
    return;
  }
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
    restoreSnapshot(bytes);
    setSnapshotStatus(`uploaded + restored ${bytes.length.toLocaleString()} bytes`);
  } catch (err) {
    setSnapshotStatus(`upload failed: ${err.message || err}`, "error");
  } finally {
    e.target.value = ""; // allow re-selecting the same file
  }
});
