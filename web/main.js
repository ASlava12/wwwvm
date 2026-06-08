// Demo wiring: WASM `WwwVm` ↔ xterm.js + control panel.
//
// Build the wasm module first (see README): wasm-pack writes the bundle
// into ./pkg/ next to this file. The page works as plain static files
// served over http (any http server — Python's http.server is fine).

import init, { WwwVm } from "./pkg/wwwvm_wasm.js";
import { saveSnapshot, loadSnapshot, listSnapshots } from "./storage.js";
import { makeBytes, breakBytes, comboBytes } from "./ps2-keymap.js?v=2";
import { SnapStore, uploadSnapshot, downloadSnapshot } from "./snapshot-store.js?v=2";
import { parseConfigFromHash, buildHashFromConfig } from "./demo-link.js";

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
// FitAddon makes the terminal fill its pane and resize with the layout (canvas
// show/hide, fullscreen, window resize) instead of a fixed 80×24 box in the
// corner. Guarded so a missing CDN addon can't break the page. NB: the guest's
// serial tty size isn't renegotiated (no SIGWINCH over UART), so the guest
// keeps emitting at its own width — this just sizes the on-screen grid.
const fitAddon = typeof FitAddon !== "undefined" ? new FitAddon.FitAddon() : null;
if (fitAddon) term.loadAddon(fitAddon);
term.open($("terminal"));
if (fitAddon) {
  const refit = () => { try { fitAddon.fit(); } catch {} };
  refit();
  new ResizeObserver(refit).observe($("terminal-pane"));
}
term.writeln("\x1b[90m(boot the VM to start)\x1b[0m");
// While the terminal is focused, keep Ctrl/Alt combos out of the browser and
// hand them to the guest instead, so e.g. Ctrl+U / Ctrl+W edit the shell line.
// (Clipboard combos stay with the browser.) Browser-RESERVED combos like Ctrl+W
// can't be cancelled in a normal tab — only the Keyboard Lock API in fullscreen
// captures those, see the Fullscreen buttons.
term.attachCustomKeyEventHandler((e) => {
  if (e.type === "keydown" && (e.ctrlKey || e.altKey) && !e.metaKey) {
    const k = (e.key || "").toLowerCase();
    if (!(e.shiftKey && (k === "c" || k === "v"))) e.preventDefault();
  }
  return true; // let xterm translate the key and send it to the guest
});

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
const worker = new Worker(new URL("./vm-worker.js?v=11", import.meta.url), { type: "module" });
let active = "inline"; // "inline" | "worker"
let useWorker = true;
let workerBooted = false;
let snapshotResolve = null;
let exportResolve = null; // pending getSnapshotExport() in worker mode

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
    case "snapshot_export":
      if (exportResolve) {
        exportResolve(m.buf ? new Uint8Array(m.buf) : null);
        exportResolve = null;
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

// Click the canvas to CAPTURE mouse + keyboard via Pointer Lock: relative mouse
// motion → PS/2 packets, keys → Set-1 scancodes. Right Alt releases the capture.
// A hint overlay reminds how to release. Routes to whichever engine owns the VM.
(function wireCanvasInput() {
  const cv = $("fb");
  const hint = $("capture-hint");
  if (!cv) return;
  let buttons = 0; // bit0 left, bit1 right, bit2 middle
  const BTN = { 0: 1, 1: 4, 2: 2 }; // DOM button (0/1/2) → our bitmask
  const wrap = document.getElementById("canvas-wrap");
  const locked = () => document.pointerLockElement === cv;
  let hovering = false;
  // The hint never sits over the desktop: the "click to capture" prompt shows
  // only while the pointer is over the canvas; the "captured" reminder flashes
  // on lock then fades.
  const refreshHint = () => {
    if (!hint) return;
    if (locked()) {
      hint.textContent = "Захвачено · ESC или правый Alt — отпустить";
      hint.classList.add("active");
      hint.style.opacity = "1";
      setTimeout(() => { if (locked()) hint.style.opacity = "0"; }, 2000);
    } else {
      hint.textContent = "Клик — захватить мышь и клавиатуру";
      hint.classList.remove("active");
      hint.style.opacity = hovering ? "1" : "0";
    }
  };

  cv.addEventListener("click", () => { if (!locked()) cv.requestPointerLock?.(); });
  document.addEventListener("pointerlockchange", refreshHint);
  wrap?.addEventListener("mouseenter", () => { hovering = true; refreshHint(); });
  wrap?.addEventListener("mouseleave", () => { hovering = false; refreshHint(); });

  // Mouse + keyboard are captured globally, but only while the canvas is locked.
  document.addEventListener("mousemove", (e) => {
    if (!locked()) return;
    const dx = e.movementX | 0, dy = -(e.movementY | 0); // screen y down; PS/2 +y up
    if (dx || dy) sendMouse(dx, dy, buttons);
  });
  document.addEventListener("mousedown", (e) => {
    if (!locked()) return;
    buttons |= BTN[e.button] || 0; sendMouse(0, 0, buttons); e.preventDefault();
  });
  document.addEventListener("mouseup", (e) => {
    if (!locked()) return;
    buttons &= ~(BTN[e.button] || 0); sendMouse(0, 0, buttons); e.preventDefault();
  });
  cv.addEventListener("contextmenu", (e) => e.preventDefault());

  document.addEventListener("keydown", (e) => {
    if (!locked()) return;
    if (e.code === "AltRight") { e.preventDefault(); document.exitPointerLock?.(); return; }
    const b = makeBytes(e.code);
    if (b) { e.preventDefault(); sendScancodes(b); }
  });
  document.addEventListener("keyup", (e) => {
    if (!locked()) return;
    if (e.code === "AltRight") { e.preventDefault(); return; }
    const b = breakBytes(e.code);
    if (b) { e.preventDefault(); sendScancodes(b); }
  });
  refreshHint();
})();

// "Send keys to VM": inject key combinations as PS/2 scan codes WITHOUT needing
// to capture the canvas — the whole point, since the browser/OS swallow combos
// like Ctrl+Alt+Del / Ctrl+Alt+F1 / Alt+Tab before they ever reach the guest.
// Quick buttons carry a "+"-joined list of DOM codes in data-combo; the
// composer builds one from the modifier checkboxes + key dropdown. Routed via
// sendScancodes (worker or inline), so it reaches the graphical guest's evdev.
(function wireKeyCombos() {
  // Populate the key dropdown (DOM code → label) so the HTML stays small.
  const keySel = $("kc-key");
  if (keySel && !keySel.options.length) {
    const L = (s) => String.fromCharCode(s);
    const KEYS = [["", "— key —"]];
    for (let c = 65; c <= 90; c++) KEYS.push(["Key" + L(c), L(c)]); // A–Z
    for (let d = 0; d <= 9; d++) KEYS.push(["Digit" + d, String(d)]); // 0–9
    for (let f = 1; f <= 12; f++) KEYS.push(["F" + f, "F" + f]); // F1–F12
    KEYS.push(
      ["Enter", "Enter"], ["Escape", "Esc"], ["Tab", "Tab"], ["Space", "Space"],
      ["Backspace", "Backspace"], ["Delete", "Delete"], ["Insert", "Insert"],
      ["Home", "Home"], ["End", "End"], ["PageUp", "PgUp"], ["PageDown", "PgDn"],
      ["ArrowUp", "↑"], ["ArrowDown", "↓"], ["ArrowLeft", "←"], ["ArrowRight", "→"],
    );
    for (const [code, label] of KEYS) {
      const o = document.createElement("option");
      o.value = code;
      o.textContent = label;
      keySel.appendChild(o);
    }
  }
  const status = $("kc-status");
  const flash = (label) => {
    if (!status) return;
    const booted = active === "worker" ? workerBooted : vm && vm.is_booted();
    status.textContent = booted ? `sent: ${label}` : "boot a guest first";
  };
  const fire = (codes, label) => {
    const bytes = comboBytes(codes);
    if (!bytes.length) return;
    sendScancodes(bytes);
    flash(label);
  };

  document.querySelectorAll("[data-combo]").forEach((btn) => {
    btn.addEventListener("click", () =>
      fire(btn.dataset.combo.split("+"), btn.textContent.trim()));
  });

  const send = $("kc-send");
  if (send) {
    send.addEventListener("click", () => {
      const codes = [];
      const labels = [];
      if ($("kc-ctrl").checked) { codes.push("ControlLeft"); labels.push("Ctrl"); }
      if ($("kc-alt").checked) { codes.push("AltLeft"); labels.push("Alt"); }
      if ($("kc-shift").checked) { codes.push("ShiftLeft"); labels.push("Shift"); }
      if ($("kc-super").checked) { codes.push("MetaLeft"); labels.push("Super"); }
      const key = $("kc-key").value;
      if (key) { codes.push(key); labels.push($("kc-key").selectedOptions[0].textContent); }
      if (!codes.length) return;
      fire(codes, labels.join("+"));
    });
  }
})();

// "Last result": click to expand into a large scrollable overlay; click the
// dimmer or press Esc to collapse. Keeps the bottom panel compact while still
// letting you read a big result comfortably.
(function wireResultExpand() {
  const res = $("last-result");
  const backdrop = $("result-backdrop");
  if (!res || !backdrop) return;
  const expand = (on) => {
    res.classList.toggle("expanded", on);
    backdrop.classList.toggle("show", on);
  };
  res.addEventListener("click", () => { if (!res.classList.contains("expanded")) expand(true); });
  backdrop.addEventListener("click", () => expand(false));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && res.classList.contains("expanded")) expand(false);
  });
})();

// Collapse/show the settings sidebar.
$("sidebar-toggle")?.addEventListener("click", () =>
  document.body.classList.toggle("sidebar-hidden"));
// Switch the single working area between the Fleet (networked VMs, default) and
// the classic single-VM workspace. Both engines keep running across switches —
// the Fleet iframe and the single VM are only hidden, never torn down. The
// button shows the OTHER mode you can switch to.
$("fleet-toggle")?.addEventListener("click", () => {
  const single = document.body.classList.toggle("mode-single");
  document.body.classList.toggle("mode-fleet", !single);
  $("fleet-toggle").textContent = single ? "🖥 Fleet" : "🖳 Single VM";
});
// Fullscreen (canvas or UART) + Keyboard Lock: in fullscreen Chromium grants
// keyboard lock so even browser-reserved combos (Ctrl+W/T/N) reach the guest.
// Firefox has no Keyboard Lock API, so there those combos can't be captured.
function toggleFullscreen(el) {
  if (document.fullscreenElement) document.exitFullscreen?.();
  else el?.requestFullscreen?.();
}
$("fullscreen-btn")?.addEventListener("click", () =>
  toggleFullscreen(document.getElementById("canvas-wrap")));
$("term-fullscreen-btn")?.addEventListener("click", () =>
  toggleFullscreen(document.getElementById("terminal-pane")));
document.addEventListener("fullscreenchange", () => {
  if (document.fullscreenElement) navigator.keyboard?.lock?.();
  else navigator.keyboard?.unlock?.();
});

let rafHandle = 0;
const outputListeners = new Set();

// Autorun for real-OS boots: once the guest's shell announces readiness, type
// the Autorun lines into it as commands. The built-in demos instead get autorun
// pre-fed at boot (no shell-ready signal), so this is the path for any kernel
// image that prints the marker. Lines are sent with a gap so the shell runs
// them in order and we don't overrun the tty's ~255-char line buffer.
let autorunListener = null;
const AUTORUN_READY_MARKER = "alpine shell ready";
function sendCommandLine(text) {
  if (active === "worker") worker.postMessage({ t: "command", text });
  else if (vm.is_booted()) vm.send_command(text);
}
function armAutorunOnReady(lines) {
  if (autorunListener) { outputListeners.delete(autorunListener); autorunListener = null; }
  if (!lines.length) return;
  let buf = "";
  autorunListener = (chunk) => {
    buf += chunk;
    if (!buf.includes(AUTORUN_READY_MARKER)) return;
    outputListeners.delete(autorunListener);
    autorunListener = null;
    let i = 0;
    const tick = () => {
      if (i >= lines.length) return;
      sendCommandLine(lines[i++]);
      setTimeout(tick, 400);
    };
    setTimeout(tick, 600); // small grace after the ready line
  };
  outputListeners.add(autorunListener);
}
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
  // First frame → reveal the canvas pane (CSS shows it under body.has-fb; the
  // UART pane shrinks to share the column). Hidden again at the next boot.
  document.body.classList.add("has-fb");
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
// Warn once if the relay can't be reached / is mixed-content-blocked, so a hung
// apk/wget has a visible cause (parity with the worker path in vm-worker.js).
let relayWarned = false;
let netProxyUrl = "ws://localhost:8080";
// Upstream-proxy selection merged into every connect frame: {} = direct,
// {auto:true} = server rotates its pool, {upstream:{kind,host,port}} = chain
// through that one specific public proxy.
let netUpstream = {};
const netConns = new Map(); // id -> { ws, open, pendingOut: [Uint8Array], pendingIn: [Uint8Array] }
const setNetStatus = (t) => { const el = $("net-status"); if (el) el.textContent = t; };

// Resolve each allowlisted host via Cloudflare DoH and seed the NAT's cache.
// Resolve one host (DoH) into the NAT's DNS cache; deduped while in flight so
// the guest's DNS retries don't spam DoH.
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
    setNetStatus(`resolved ${host} → ${ips.join(", ") || "(none)"} (${kept} cached)`);
  } catch (e) {
    setNetStatus(`DoH resolve failed for ${host}: ${e.message || e}`);
  } finally {
    dnsInFlight.delete(host);
  }
}

// Pre-resolve the allowlist's named hosts (skip "*"/empty). With allow-all,
// names are resolved on demand from pumpNet instead.
async function netPreResolve(allowlist) {
  const hosts = [...new Set(allowlist.split(/[,\n]/)
    .map((s) => s.trim().split(":")[0]).filter(Boolean))];
  for (const host of hosts) {
    if (host !== "*") resolveHost(host);
  }
}

// Read the upstream-proxy choice from the UI into a connect-frame fragment.
// A non-empty manual field wins; otherwise the dropdown (direct / auto / a
// fetched proxy encoded as "kind|host|port"). Returns {} for direct/invalid.
function readUpstream() {
  const status = $("net-upstream-status");
  const manual = $("net-upstream-manual").value.trim();
  if (manual) {
    const m = manual.match(/^(?:(socks5|socks4|http):\/\/)?([^\s:/]+):(\d+)$/i);
    if (!m) {
      if (status) status.textContent = `⚠ bad upstream "${manual}" — use kind://host:port`;
      return {};
    }
    return { upstream: { kind: (m[1] || "socks5").toLowerCase(), host: m[2], port: +m[3] } };
  }
  const v = $("net-upstream").value;
  if (v === "auto") return { auto: true };
  if (v === "direct" || !v) return {};
  const [kind, host, port] = v.split("|");
  return { upstream: { kind, host, port: +port } };
}

// Populate the upstream dropdown from proxies.json (the cron parser's output).
// Absent/empty file → just the built-in Direct + Auto options.
async function loadProxyList() {
  const sel = $("net-upstream");
  const status = $("net-upstream-status");
  try {
    const r = await fetch("proxies.json", { cache: "no-cache" });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const j = await r.json();
    const list = Array.isArray(j.proxies) ? j.proxies : [];
    if (!list.length) throw new Error("list is empty");
    const grp = document.createElement("optgroup");
    grp.label = `public proxies (${list.length})`;
    for (const p of list) {
      const o = document.createElement("option");
      o.value = `${p.type}|${p.host}|${p.port}`;
      o.textContent = `${p.type} ${p.host}:${p.port}`;
      grp.appendChild(o);
    }
    sel.appendChild(grp);
    if (status) status.textContent =
      `${list.length} public proxies loaded — untrusted, non-sensitive traffic only`;
  } catch (e) {
    if (status) status.textContent =
      `no proxies.json (run scripts/fetch-proxies.py) — Direct/Auto/manual still work`;
  }
}

function netOpenConn({ id, host, port }) {
  // hostClosed: the WebSocket ended — stop reading, but keep the slot until
  // every buffered inbound chunk has been handed to the guest (else the tail
  // of a response is dropped and the guest is FIN'd early).
  const c = { ws: null, open: false, hostClosed: false, pendingIn: [] };
  netConns.set(id, c);
  // An https page can't open a ws:// relay (mixed content, silently blocked).
  if (!relayWarned && location.protocol === "https:" && /^ws:\/\//i.test(netProxyUrl)) {
    relayWarned = true;
    setNetStatus(`relay is ws:// but the page is https — blocked as mixed content; use a wss:// relay`);
  }
  let ws;
  try {
    ws = new WebSocket(netProxyUrl);
  } catch (e) {
    if (!relayWarned) {
      relayWarned = true;
      setNetStatus(`bad relay URL "${netProxyUrl}": ${e.message || e}`);
    }
    vm.net_conn_closed(id);
    netConns.delete(id);
    return;
  }
  ws.binaryType = "arraybuffer";
  c.ws = ws;
  ws.onopen = () => {
    c.open = true;
    // proxy handshake header; netUpstream adds {auto} or {upstream:{…}} if set
    ws.send(JSON.stringify({ host, port, ...netUpstream }));
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
    // Never opened → the relay is unreachable (not started / wrong URL / blocked).
    if (!c.open && !relayWarned) {
      relayWarned = true;
      setNetStatus(`relay unreachable at ${netProxyUrl} — start it (cargo run -p wwwvm-proxy) and check its allowlist + WWWVM_PROXY_ORIGINS`);
    }
    c.hostClosed = true; // drained + torn down in pumpNet once pendingIn empties
  };
}

function pumpNet() {
  if (!netEnabled) return;
  vm.net_pump(performance.now());

  // On-demand DNS: resolve names the guest queried that weren't pre-cached
  // (makes an allow-all "*" list work).
  let dnsReqs;
  try { dnsReqs = JSON.parse(vm.net_take_dns_requests()); } catch { dnsReqs = []; }
  for (const name of dnsReqs) resolveHost(name);

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
  document.body.classList.remove("has-fb"); // built-in guests have no framebuffer
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

// Seed the guest CMOS clock with the host's real time (UTC) so the guest's
// `date` is correct instead of the 2026-01-01 default — wasm has no host clock,
// so pass JS Date components (year two-digit, month 1-12). The guest treats the
// RTC as UTC and runs on UTC; that's fine — the point is a correct clock.
function seedGuestClock(vm) {
  if (typeof vm.set_cmos_time !== "function") return;
  const d = new Date();
  vm.set_cmos_time(
    d.getUTCFullYear() % 100, d.getUTCMonth() + 1, d.getUTCDate(),
    d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds()
  );
}

// Boot a real Linux/Alpine kernel from in-memory buffers (kernel bzImage +
// optional initramfs cpio as ArrayBuffers). Shared by the server-image picker
// and the "load your own files" fallback. cmdline / framebuffer / networking
// are read from the control panel (the image picker fills those in first).
// `ramMiB` sizes the guest RAM — the GUI image asks for more (framebuffer +
// the DRM/input modules unpacked into the initramfs tmpfs).
async function bootLinux(kbuf, ibuf, { ramMiB = 256 } = {}) {
  if (rafHandle) cancelAnimationFrame(rafHandle);
  // Hide the canvas until this boot actually produces a frame (paintFb re-shows
  // it). A console-only image never does, so the UART fills the column.
  document.body.classList.remove("has-fb");
  // Autorun: type these into the guest once its shell is ready (any boot path).
  // (The guest runs on UTC — its RTC is seeded with real UTC time.)
  armAutorunOnReady($("autorun").value.split("\n").map((s) => s.trim()).filter(Boolean));
  useWorker = $("worker-enable")?.checked ?? true;
  // RAM (MiB) + optional tmpfs RAM disk are set in the UI; the RAM input
  // overrides the image default, the RAM disk appends `wwwvm.ramdisk=N` to the
  // cmdline (the guest /init mounts it at /mnt/ramdisk).
  const ramEl = $("vm-ram");
  const v = ramEl ? parseInt(ramEl.value, 10) : NaN;
  if (v >= 64) ramMiB = v;
  const rd = parseInt($("vm-ramdisk")?.value, 10) || 0;
  const cmdline = $("cmdline").value + (rd > 0 ? ` wwwvm.ramdisk=${rd}` : "");
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
          upstream: readUpstream(), // {} | {auto} | {upstream:{…}}
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
    seedGuestClock(vm);
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
    relayWarned = false;
    netEnabled = $("net-enable").checked;
    if (netEnabled) {
      netProxyUrl = $("net-proxy").value.trim() || "ws://localhost:8080";
      netUpstream = readUpstream(); // {} | {auto} | {upstream:{…}}
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
  if ($("vm-ram") && img.ramMiB) $("vm-ram").value = img.ramMiB; // image's RAM default (overridable)
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
      // Content-keyed query busts the browser cache when the image is rebuilt
      // (manifest is always revalidated, so img.bytes is current).
      fetch(IMAGES_BASE + img.initramfs + `?b=${img.bytes || 0}`),
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

// Shareable demo links: apply a `#img=…&autorun=…&net=1&boot=1` hash to the
// controls once the image list is in (so an `img` id can select its <option>),
// and a "Share" button that captures the current controls back into such a
// link. Lets a whole demo scenario travel as one URL. See demo-link.js.
function applyDemoLinkFromHash() {
  const cfg = parseConfigFromHash(location.hash);
  if (!Object.keys(cfg).length) return;
  // An image id selects its option + its defaults FIRST, so explicit hash
  // fields below win over the image's cmdline/fb/ram defaults.
  let imgFound = false;
  if (cfg.img) {
    const img = imageManifest.find((x) => x.id === cfg.img);
    if (img) {
      $("image-select").value = cfg.img;
      applyImageToControls(img);
      imgFound = true;
    } else {
      $("image-status").textContent =
        `shared link wants image "${cfg.img}", which this server doesn't have`;
    }
  }
  if (cfg.cmdline !== undefined) $("cmdline").value = cfg.cmdline;
  if (cfg.ram && $("vm-ram")) $("vm-ram").value = cfg.ram;
  if (cfg.ramdisk && $("vm-ramdisk")) $("vm-ramdisk").value = cfg.ramdisk;
  if (cfg.fb !== undefined) $("fb-enable").checked = cfg.fb === "1";
  if (cfg.fbres) {
    const sel = $("fb-res");
    if (![...sel.options].some((o) => o.value === cfg.fbres)) {
      const o = document.createElement("option");
      o.value = cfg.fbres;
      o.textContent = cfg.fbres.replace("x", "×");
      sel.appendChild(o);
    }
    sel.value = cfg.fbres;
  }
  if (cfg.net !== undefined) $("net-enable").checked = cfg.net === "1";
  if (cfg.allow !== undefined) $("net-allow").value = cfg.allow;
  if (cfg.autorun !== undefined) $("autorun").value = cfg.autorun;

  if (cfg.boot === "1") {
    // Auto-boot the scenario: the named server image when this server has it
    // (and the button is live), otherwise the built-in "Boot VM". Don't boot
    // some *other* image just because the link asked for one we lack.
    if (imgFound && !$("boot-image").disabled) $("boot-image").click();
    else if (!cfg.img) $("boot").click();
  }
}

// Capture the current Linux/Alpine controls into a shareable hash and copy it.
function shareDemoLink() {
  const cfg = {
    img: $("image-select").value || "",
    cmdline: $("cmdline").value,
    ram: $("vm-ram")?.value || "",
    ramdisk: ($("vm-ramdisk")?.value || "0") !== "0" ? $("vm-ramdisk").value : "",
    fb: $("fb-enable").checked,
    fbres: $("fb-res").value,
    net: $("net-enable").checked,
    allow: $("net-enable").checked ? $("net-allow").value.trim() : "",
    autorun: $("autorun").value.trim(),
    boot: true, // a demo link auto-boots the scenario on open
  };
  const hash = buildHashFromConfig(cfg);
  const url = location.origin + location.pathname + hash;
  history.replaceState(null, "", hash || location.pathname);
  const note = (msg) => {
    const el = $("share-status");
    if (el) el.textContent = msg;
  };
  navigator.clipboard?.writeText(url).then(
    () => note("link copied to clipboard ✓"),
    () => note("link is in the address bar (copy it)"),
  );
}
$("share-link")?.addEventListener("click", shareDemoLink);

loadImageManifest().then(applyDemoLinkFromHash);
loadProxyList();

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

// Strip ANSI escapes (colors from `ls --color`, cursor moves, OSC titles) and
// normalize CRLF — the terminal renders these, but the plain-text result box
// would otherwise show them raw as "[1;34m…[m".
function stripAnsi(s) {
  return String(s)
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "") // OSC (e.g. window title)
    .replace(/\x1b[[\x9b][0-9;?=]*[ -/]*[@-~]/g, "") // CSI (color, cursor, …)
    .replace(/\x1b[@-Z\\-_]/g, "") // other single-char escapes
    .replace(/\r\n?/g, "\n"); // CRLF / lone CR → LF
}

$("send").addEventListener("click", async () => {
  const text = $("cmd").value;
  if (!text) return;
  try {
    const result = await runCommand(text);
    $("last-result").textContent = stripAnsi(result) || "(no output)";
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

// Paged-export variants (for the custom-snapshot store). snapshot_export()
// returns the manifest + RAM pages framed for content-addressed upload;
// restore_export() takes a rebuilt export buffer.
async function getSnapshotExport() {
  if (active === "worker") {
    return await new Promise((resolve) => {
      exportResolve = resolve;
      worker.postMessage({ t: "snapshot_export" });
    });
  }
  return vm.snapshot_export();
}
function restoreSnapshotExport(bytes) {
  if (active === "worker") {
    worker.postMessage({ t: "restore_export", buf: bytes.buffer }, [bytes.buffer]);
    term.reset();
  } else {
    vm.restore_export(bytes);
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

// ---- Custom snapshots: recipe → content-addressed store (crates/snapstore) ----
const snapStore = () =>
  new SnapStore($("snap-url").value.trim(), $("snap-token").value.trim());
const setSnapStatus = (t, cls) => {
  const e = $("snap-status");
  if (e) {
    e.textContent = t;
    e.className = "status" + (cls ? " " + cls : "");
  }
};

$("snap-build")?.addEventListener("click", async () => {
  if (!isBooted()) return setSnapStatus("boot a base image first", "error");
  const name = $("snap-name").value.trim();
  if (!name) return setSnapStatus("enter a snapshot name", "error");
  const recipe = $("snap-recipe").value.split("\n").map((s) => s.trim()).filter(Boolean);
  const btn = $("snap-build");
  btn.disabled = true;
  try {
    // Run the recipe on the booted base. runCommand waits for output to settle;
    // a long step (e.g. apk add) may exceed the timeout — keep recipes modest or
    // raise it. The user controls the recipe and can verify the console.
    setSnapStatus(`running recipe (${recipe.length} cmd)…`);
    for (const line of recipe) await runCommand(line, 8000);
    setSnapStatus("snapshotting…");
    const buf = await getSnapshotExport();
    if (!buf) throw new Error("no snapshot returned");
    const r = await uploadSnapshot(snapStore(), name, buf, (done, total) => {
      if (done % 64 === 0 || done === total) setSnapStatus(`uploading pages ${done}/${total}…`);
    });
    setSnapStatus(
      `uploaded "${name}": ${r.uploaded}/${r.pages} pages new (${(r.bytes / 1048576).toFixed(1)} MiB)`,
    );
  } catch (e) {
    setSnapStatus(`build failed: ${e.message || e}`, "error");
  } finally {
    btn.disabled = false;
  }
});

$("snap-refresh")?.addEventListener("click", async () => {
  try {
    const ids = await snapStore().listManifests();
    const sel = $("snap-list");
    sel.innerHTML = ids.length ? "" : '<option value="">(none)</option>';
    for (const id of ids) {
      const o = document.createElement("option");
      o.value = o.textContent = id;
      sel.appendChild(o);
    }
    setSnapStatus(`${ids.length} snapshot(s) in store`);
  } catch (e) {
    setSnapStatus(`list failed: ${e.message || e}`, "error");
  }
});

$("snap-load")?.addEventListener("click", async () => {
  const id = $("snap-list").value;
  if (!id) return setSnapStatus("pick a snapshot (Refresh first)", "error");
  try {
    setSnapStatus(`downloading "${id}"…`);
    const buf = await downloadSnapshot(snapStore(), id);
    restoreSnapshotExport(buf);
    setSnapStatus(`restored "${id}" (${(buf.length / 1048576).toFixed(1)} MiB)`);
  } catch (e) {
    setSnapStatus(`load failed: ${e.message || e}`, "error");
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
