// Virtual LAN lab: run several VMs in parallel (one Web Worker each) and wire
// their NICs together through the in-page L2 learning switch, so they form a
// real Ethernet segment and can talk to each other (ARP, ping, any protocol).
// Mirrors the native crates/net Hub: drain each VM's transmitted frames, route
// via L2Switch, inject into the destination VM(s).
//
// Each VM gets a FULL interactive xterm.js terminal (same as the single-VM
// workspace): keystrokes are sent raw to the guest UART ({t:"input"}), output
// is written verbatim ({t:"output"}) so ANSI colours, line editing, Ctrl+C, etc.
// all work — no separate "type a command" field.
//
// Two modes (the "Internet" checkbox):
//   • peer-only — 10.0.0.0/24, no gateway (worker net mode "switch").
//   • hybrid    — 10.0.2.0/24 with an in-wasm NAT gateway at 10.0.2.2 per VM
//                 (worker mode "lan+nat"): each VM is on the switch for peers
//                 AND reaches the outside world through your WebSocket relay.
//
// Needs a "LAN" guest image (WWWVM_NET_LAN → /init reads its IP from the kernel
// cmdline `wwwvm.ip=10.0.2.N/24`, and, when `wwwvm.gw=` is present, the gateway
// + DNS + apk-http). Each worker gets a distinct MAC (set_nic_mac) + IP.

import { L2Switch } from "./l2-switch.js?v=1";

const $ = (id) => document.getElementById(id);
const IMAGES_BASE = "images/";

let workers = []; // index → Worker (the index is also the switch port number)
let terms = []; // index → xterm Terminal
let fits = []; // index → FitAddon
let meta = []; // index → { ip, ram, ramdisk }
let active = 0; // index of the focused VM
let sw = new L2Switch();
let manifest = [];
// Snapshot of the launch config so "+ Add VM" reuses the same image/subnet/net.
let lanCfg = null; // { netOn, allow, proxy, base, kernel, initrd, imgName }

const ipOf = (i) => (lanCfg && lanCfg.netOn ? `10.0.2.${15 + i}` : `10.0.0.${i + 1}`);
const setStatus = (t) => { $("lan-status").textContent = t; };

function fmtBytes(b) {
  if (b < 1024) return `${b}B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)}K`;
  return `${(b / 1024 / 1024).toFixed(1)}M`;
}
function fmtUp(ms) {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function setVmState(i, label, up) {
  const st = $(`st-${i}`);
  if (st) {
    st.textContent = label;
    st.classList.toggle("up", !!up);
  }
  const head = $(`head-${i}`);
  if (head) head.textContent = label;
}

// Live per-VM stats (uptime + bytes through the NIC + open NAT flows).
function setVmStat(i, m) {
  const el = $(`stat-${i}`);
  if (!el) return;
  el.textContent =
    `↑${fmtBytes(m.txBytes)} ↓${fmtBytes(m.rxBytes)} · ${fmtUp(m.upMs)}` +
    (m.flows ? ` · ${m.flows} flow${m.flows === 1 ? "" : "s"}` : "");
}

// Fit the focused VM's terminal to its pane (after a show / resize).
function fitActive() {
  const f = fits[active];
  if (f) { try { f.fit(); } catch {} }
}

// Focus VM `i`: show its pane, hide the others, highlight its list entry, then
// size + focus its terminal so typing goes straight to that guest.
function activate(i) {
  active = i;
  for (let k = 0; k < workers.length; k++) {
    $(`pane-${k}`)?.classList.toggle("active", k === i);
    $(`item-${k}`)?.classList.toggle("sel", k === i);
  }
  fitActive();
  try { terms[i]?.focus(); } catch {}
}

function stopLan() {
  for (const w of workers) {
    try { w.terminate(); } catch {}
  }
  for (const t of terms) {
    try { t?.dispose(); } catch {}
  }
  workers = [];
  terms = [];
  fits = [];
  meta = [];
  lanCfg = null;
  sw = new L2Switch();
  $("vm-stage").innerHTML = "";
  $("vm-list").querySelectorAll(".vm-item").forEach((el) => el.remove());
}

async function loadManifest() {
  try {
    const r = await fetch(IMAGES_BASE + "manifest.json", { cache: "no-cache" });
    const j = await r.json();
    manifest = Array.isArray(j.images) ? j.images : [];
  } catch {
    manifest = [];
  }
  const sel = $("lan-image");
  sel.innerHTML = "";
  if (!manifest.length) {
    sel.innerHTML = '<option value="">(no images — run scripts/build-web-images.sh)</option>';
    return;
  }
  for (const img of manifest) {
    const o = document.createElement("option");
    o.value = img.id;
    o.textContent = (img.name || img.id) + (img.lan ? " (LAN)" : "");
    sel.appendChild(o);
  }
  const lan = manifest.find((m) => m.lan);
  if (lan) sel.value = lan.id;
}

// Route a frame VM `i` transmitted to its destination port(s) via the switch.
function relayTx(i, frames) {
  for (const f of frames) {
    const u = new Uint8Array(f);
    for (const eg of sw.egress(i, u, workers.length)) {
      const copy = u.slice(); // fresh buffer per target (transfer consumes it)
      workers[eg].postMessage({ t: "rx", frame: copy.buffer }, [copy.buffer]);
    }
  }
}

function wireWorker(i, worker) {
  worker.onmessage = (e) => {
    const m = e.data;
    switch (m.t) {
      case "output": terms[i]?.write(m.text); break;
      case "booted": setVmState(i, "up", true); break;
      case "tx": relayTx(i, m.frames); break;
      case "stat": setVmStat(i, m); break;
      case "error": terms[i]?.write(`\r\n[error] ${m.message}\r\n`); setVmState(i, "error"); break;
    }
  };
}

// Create the interactive terminal for VM `i` inside its pane and wire keystrokes
// to that VM's worker (raw UART input). Mirrors the single-VM workspace.
function makeTerminal(i) {
  const el = $(`term-${i}`);
  if (typeof Terminal === "undefined") {
    if (el) el.textContent = "(xterm.js failed to load — check the CDN)";
    return;
  }
  const term = new Terminal({
    fontFamily: "ui-monospace, monospace",
    fontSize: 12,
    theme: { background: "#000000", foreground: "#d6dde6" },
    cursorBlink: true,
    convertEol: true,
    scrollback: 5000,
  });
  const fit = typeof FitAddon !== "undefined" ? new FitAddon.FitAddon() : null;
  if (fit) term.loadAddon(fit);
  term.open(el);
  if (fit) { try { fit.fit(); } catch {} }
  // Browser combos (Ctrl/Alt) go to the guest, not the page (except copy/paste).
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type === "keydown" && (ev.ctrlKey || ev.altKey) && !ev.metaKey) {
      const k = (ev.key || "").toLowerCase();
      if (!(ev.shiftKey && (k === "c" || k === "v"))) ev.preventDefault();
    }
    return true;
  });
  term.onData((d) => {
    if (!workers[i]) return;
    const b = new TextEncoder().encode(d);
    workers[i].postMessage({ t: "input", bytes: b }, [b.buffer]);
  });
  terms[i] = term;
  fits[i] = fit;
}

// Build the focused pane (in #vm-stage) + the right-hand list entry (#vm-list).
function addPane(i) {
  const stage = $("vm-stage");
  const list = $("vm-list");

  const pane = document.createElement("div");
  pane.className = "vm-pane";
  pane.id = `pane-${i}`;
  pane.innerHTML =
    `<div class="vm-head">VM ${i + 1} · ${ipOf(i)} · <span id="head-${i}">booting…</span></div>` +
    `<div class="vm-term" id="term-${i}"></div>`;
  stage.appendChild(pane);
  makeTerminal(i);

  const item = document.createElement("button");
  item.className = "vm-item";
  item.id = `item-${i}`;
  item.innerHTML =
    `VM ${i + 1} <span class="st" id="st-${i}">booting…</span><br>` +
    `<span class="ip">${ipOf(i)}</span><br><span class="ip" id="stat-${i}"></span>`;
  item.addEventListener("click", () => activate(i));
  list.appendChild(item);
}

// Spin up one more VM on the running LAN, using the current RAM / RAM disk
// inputs (so consecutive adds can be sized differently).
function addOneVm() {
  if (!lanCfg) { setStatus("Start the LAN first"); return -1; }
  if (workers.length >= 8) { setStatus("max 8 VMs on the lab switch"); return -1; }
  const i = workers.length;
  const ram = Math.max(64, parseInt($("lan-ram").value, 10) || 256);
  const ramdisk = Math.max(0, parseInt($("lan-ramdisk").value, 10) || 0);
  meta[i] = { ip: ipOf(i), ram, ramdisk };

  addPane(i);
  const worker = new Worker(new URL("./vm-worker.js?v=10", import.meta.url), { type: "module" });
  wireWorker(i, worker);
  workers.push(worker);

  const mac = [0x52, 0x54, 0x00, 0x00, 0x00, i + 1];
  const rd = ramdisk > 0 ? ` wwwvm.ramdisk=${ramdisk}` : "";
  const gw = lanCfg.netOn ? " wwwvm.gw=10.0.2.2" : "";
  const cmdline = `${lanCfg.base} wwwvm.ip=${ipOf(i)}/24${gw}${rd}`;
  const net = lanCfg.netOn
    ? {
        mode: "lan+nat",
        mac,
        ip: ipOf(i).split(".").map(Number),
        allow: lanCfg.allow,
        proxyUrl: lanCfg.proxy,
        upstream: {},
      }
    : { mode: "switch", mac };

  worker.postMessage({
    t: "boot",
    linux: true,
    kernel: lanCfg.kernel, // structured-clone copy per worker (no transfer)
    initrd: lanCfg.initrd,
    cmdline,
    fb: null,
    net,
    ramMiB: ram,
  });
  return i;
}

async function startLan() {
  const n = Math.max(1, Math.min(8, parseInt($("lan-count").value, 10) || 2));
  const img = manifest.find((m) => m.id === $("lan-image").value);
  if (!img) { setStatus("pick an image (build a LAN image: scripts/build-web-images.sh)"); return; }

  stopLan();
  setStatus(`fetching ${img.name}…`);
  let kernel, initrd;
  try {
    const [kr, ir] = await Promise.all([
      fetch(IMAGES_BASE + img.kernel),
      fetch(IMAGES_BASE + img.initramfs),
    ]);
    kernel = await kr.arrayBuffer();
    initrd = await ir.arrayBuffer();
  } catch (e) {
    setStatus(`image fetch failed: ${e.message || e}`);
    return;
  }

  const netOn = $("lan-net").checked;
  lanCfg = {
    netOn,
    allow: ($("lan-allow").value || "*").trim() || "*",
    proxy: ($("lan-proxy").value || "ws://localhost:8080").trim() || "ws://localhost:8080",
    base: img.cmdline || "console=ttyS0 panic=10 loglevel=4",
    kernel,
    initrd,
    imgName: img.name || img.id,
  };

  for (let k = 0; k < n; k++) addOneVm();
  activate(0);
  setStatus(
    `LAN up: ${n} VM(s) on ${netOn ? "10.0.2.0/24 + NAT internet" : "10.0.0.0/24 (peer-only)"}` +
    " — click a VM on the right, type in its terminal; `+ Add VM` grows the LAN. " +
    (netOn
      ? "Internet via your relay (TCP: `apk update`, `wget`); `ping 10.0.2.2` hits the gateway."
      : `Try \`ping -c2 ${ipOf(1)}\` in VM 1.`),
  );
}

$("lan-start").addEventListener("click", startLan);
$("lan-add").addEventListener("click", () => {
  const i = addOneVm();
  if (i >= 0) {
    activate(i);
    setStatus(`added VM ${i + 1} (${ipOf(i)}) — ${workers.length} VM(s) running`);
  }
});
$("lan-stop").addEventListener("click", () => { stopLan(); setStatus("stopped"); });
$("vm-list-toggle").addEventListener("click", () => {
  $("vm-list").classList.toggle("collapsed");
  fitActive(); // the console widened/narrowed — resize the terminal to match
});
// Keep the focused terminal sized to its pane as the layout changes.
if (typeof ResizeObserver !== "undefined") {
  new ResizeObserver(() => fitActive()).observe($("vm-stage"));
}
loadManifest();
