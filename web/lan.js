// Virtual LAN lab: run several VMs in parallel (one Web Worker each) and wire
// their NICs together through the in-page L2 learning switch, so they form a
// real Ethernet segment and can talk to each other (ARP, ping, any protocol) —
// no NAT, no outside world. Mirrors the native crates/net Hub: drain each VM's
// transmitted frames, route via L2Switch, inject into the destination VM(s).
//
// UI: a focused-VM stage on the left + an openable list of running VMs on the
// right; click a VM to view it. Per-launch you can set RAM and a tmpfs RAM disk.
//
// Needs a "LAN" guest image (WWWVM_NET_LAN → /init reads its IP from the kernel
// cmdline `wwwvm.ip=10.0.0.N/24`, with NIC modules so eth0 exists). Each worker
// gets a distinct MAC (set_nic_mac) + IP (cmdline).

import { L2Switch } from "./l2-switch.js?v=1";

const $ = (id) => document.getElementById(id);
const IMAGES_BASE = "images/";

let workers = [];
let active = 0; // index of the focused VM
const sw = new L2Switch();
let manifest = [];

const stripAnsi = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "").replace(/\x1b[()][AB0]/g, "");
const ipOf = (i) => `10.0.0.${i + 1}`;
const setStatus = (t) => { $("lan-status").textContent = t; };

function appendConsole(i, text) {
  const pre = $(`con-${i}`);
  if (!pre) return;
  const atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
  pre.textContent = (pre.textContent + stripAnsi(text)).slice(-20000);
  if (atBottom) pre.scrollTop = pre.scrollHeight;
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

// Focus VM `i`: show its pane, hide the others, highlight its list entry.
function activate(i) {
  active = i;
  for (let k = 0; k < workers.length; k++) {
    $(`pane-${k}`)?.classList.toggle("active", k === i);
    $(`item-${k}`)?.classList.toggle("sel", k === i);
  }
  $(`cmd-${i}`)?.focus();
}

function stopLan() {
  for (const w of workers) {
    try { w.terminate(); } catch {}
  }
  workers = [];
  $("vm-stage").innerHTML = "";
  // Leave only the header in the list.
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
      case "output": appendConsole(i, m.text); break;
      case "booted": setVmState(i, "booted", true); break;
      case "tx": relayTx(i, m.frames); break;
      case "error": appendConsole(i, `\n[error] ${m.message}\n`); setVmState(i, "error"); break;
    }
  };
}

// Build the focused panes (in #vm-stage) + the right-hand list (#vm-list).
function buildUi(n) {
  const stage = $("vm-stage");
  const list = $("vm-list");
  stage.innerHTML = "";
  list.querySelectorAll(".vm-item").forEach((el) => el.remove());

  for (let i = 0; i < n; i++) {
    const pane = document.createElement("div");
    pane.className = "vm-pane";
    pane.id = `pane-${i}`;
    pane.innerHTML =
      `<div class="vm-head">VM ${i + 1} · ${ipOf(i)} · <span id="head-${i}">booting…</span></div>` +
      `<pre class="vm-con" id="con-${i}"></pre>` +
      `<div class="vm-row"><input id="cmd-${i}" placeholder="shell cmd in VM ${i + 1} (e.g. ping -c2 ${ipOf(i === 0 ? 1 : 0)})" /></div>`;
    stage.appendChild(pane);
    const inp = pane.querySelector(`#cmd-${i}`);
    inp.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" && workers[i] && inp.value) {
        workers[i].postMessage({ t: "command", text: inp.value });
        inp.value = "";
      }
    });

    const item = document.createElement("button");
    item.className = "vm-item";
    item.id = `item-${i}`;
    item.innerHTML =
      `VM ${i + 1} <span class="st" id="st-${i}">booting…</span><br><span class="ip">${ipOf(i)}</span>`;
    item.addEventListener("click", () => activate(i));
    list.appendChild(item);
  }
  activate(0);
}

async function startLan() {
  const n = Math.max(1, Math.min(8, parseInt($("lan-count").value, 10) || 2));
  const ramMiB = Math.max(64, parseInt($("lan-ram").value, 10) || 256);
  const ramdisk = Math.max(0, parseInt($("lan-ramdisk").value, 10) || 0);
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

  buildUi(n);
  const base = img.cmdline || "console=ttyS0 panic=10 loglevel=4";
  const rd = ramdisk > 0 ? ` wwwvm.ramdisk=${ramdisk}` : "";
  for (let i = 0; i < n; i++) {
    const worker = new Worker(new URL("./vm-worker.js?v=7", import.meta.url), { type: "module" });
    wireWorker(i, worker);
    workers.push(worker);
    worker.postMessage({
      t: "boot",
      linux: true,
      kernel, // structured-clone copy per worker (no transfer)
      initrd,
      cmdline: `${base} wwwvm.ip=${ipOf(i)}/24${rd}`,
      fb: null,
      net: { mode: "switch", mac: [0x52, 0x54, 0x00, 0x00, 0x00, i + 1] },
      ramMiB,
    });
  }
  activate(0);
  setStatus(
    `LAN up: ${n} VM(s) on 10.0.0.0/24, ${ramMiB} MiB each` +
    (ramdisk ? ` + ${ramdisk} MiB /mnt/ramdisk` : "") +
    " — pick a VM on the right; try `ping -c2 10.0.0.2` in VM 1",
  );
}

$("lan-start").addEventListener("click", startLan);
$("lan-stop").addEventListener("click", () => { stopLan(); setStatus("stopped"); });
$("vm-list-toggle").addEventListener("click", () => $("vm-list").classList.toggle("collapsed"));
loadManifest();
