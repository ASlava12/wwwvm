// Virtual LAN lab: run several VMs in parallel (one Web Worker each) and wire
// their NICs together through the in-page L2 learning switch, so they form a
// real Ethernet segment and can talk to each other (ARP, ping, any protocol) —
// no NAT, no outside world. This is the browser hub that mirrors the native
// crates/net Hub: drain each VM's transmitted frames, route via L2Switch, inject
// into the destination VM(s).
//
// Needs a "LAN" guest image (built with WWWVM_NET_LAN so /init reads its IP from
// the kernel cmdline `wwwvm.ip=10.0.0.N/24`, and with the NIC modules so eth0
// exists). Each worker gets a distinct MAC (set_nic_mac) + IP (cmdline).

import { L2Switch } from "./l2-switch.js?v=1";

const $ = (id) => document.getElementById(id);
const IMAGES_BASE = "images/";

let workers = [];
const sw = new L2Switch();
let manifest = [];

const stripAnsi = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "").replace(/\x1b[()][AB0]/g, "");

function setStatus(t) {
  $("lan-status").textContent = t;
}

function appendConsole(i, text) {
  const pre = $(`con-${i}`);
  if (!pre) return;
  pre.textContent = (pre.textContent + stripAnsi(text)).slice(-6000);
  pre.scrollTop = pre.scrollHeight;
}

// Tear down any running LAN.
function stopLan() {
  for (const w of workers) {
    try { w.terminate(); } catch {}
  }
  workers = [];
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
  // Prefer a LAN-tagged image if present.
  const lan = manifest.find((m) => m.lan);
  if (lan) sel.value = lan.id;
}

// Route a frame the VM on port `i` transmitted to its destination port(s).
function relayTx(i, frames) {
  for (const f of frames) {
    const u = new Uint8Array(f);
    for (const eg of sw.egress(i, u, workers.length)) {
      const copy = u.slice(); // a fresh buffer per target (transfer consumes it)
      workers[eg].postMessage({ t: "rx", frame: copy.buffer }, [copy.buffer]);
    }
  }
}

function wireWorker(i, worker) {
  worker.onmessage = (e) => {
    const m = e.data;
    switch (m.t) {
      case "output": appendConsole(i, m.text); break;
      case "booted": $(`vm-${i}-state`).textContent = "booted"; break;
      case "tx": relayTx(i, m.frames); break;
      case "error": appendConsole(i, `\n[error] ${m.message}\n`); break;
    }
  };
}

function buildConsoles(n) {
  const grid = $("lan-grid");
  grid.innerHTML = "";
  for (let i = 0; i < n; i++) {
    const ip = `10.0.0.${i + 1}`;
    const cell = document.createElement("div");
    cell.className = "vm-cell";
    cell.innerHTML =
      `<div class="vm-head">VM ${i + 1} · ${ip} · <span id="vm-${i}-state">booting…</span></div>` +
      `<pre class="vm-con" id="con-${i}"></pre>` +
      `<div class="vm-row"><input id="cmd-${i}" placeholder="shell cmd (e.g. ping -c2 10.0.0.${i === 0 ? 2 : 1})" /></div>`;
    grid.appendChild(cell);
    // Enter in a VM's input box sends the line to that VM's UART.
    setTimeout(() => {
      const inp = $(`cmd-${i}`);
      inp?.addEventListener("keydown", (ev) => {
        if (ev.key === "Enter" && workers[i]) {
          workers[i].postMessage({ t: "command", text: inp.value });
          inp.value = "";
        }
      });
    }, 0);
  }
}

async function startLan() {
  const n = Math.max(2, Math.min(8, parseInt($("lan-count").value, 10) || 2));
  const imgId = $("lan-image").value;
  const img = manifest.find((m) => m.id === imgId);
  if (!img) { setStatus("pick an image (build one with --with-lan)"); return; }

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

  buildConsoles(n);
  const baseCmdline = img.cmdline || "console=ttyS0 panic=10 loglevel=4";
  for (let i = 0; i < n; i++) {
    const worker = new Worker(new URL("./vm-worker.js?v=7", import.meta.url), { type: "module" });
    wireWorker(i, worker);
    workers.push(worker);
    worker.postMessage({
      t: "boot",
      linux: true,
      kernel, // structured-clone copy per worker (no transfer)
      initrd,
      cmdline: `${baseCmdline} wwwvm.ip=10.0.0.${i + 1}/24`,
      fb: null,
      net: { mode: "switch", mac: [0x52, 0x54, 0x00, 0x00, 0x00, i + 1] },
      ramMiB: img.ramMiB || 256,
    });
  }
  setStatus(`LAN up: ${n} VMs on 10.0.0.0/24 — wait for boot, then e.g. \`ping -c2 10.0.0.2\` in VM 1`);
}

$("lan-start").addEventListener("click", startLan);
$("lan-stop").addEventListener("click", () => { stopLan(); setStatus("stopped"); });
loadManifest();
