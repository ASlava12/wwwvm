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
