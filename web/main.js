// Demo wiring: WASM `WwwVm` ↔ xterm.js + control panel.
//
// Build the wasm module first (see README): wasm-pack writes the bundle
// into ./pkg/ next to this file. The page works as plain static files
// served over http (any http server — Python's http.server is fine).

import init, { WwwVm } from "./pkg/wwwvm_wasm.js";

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
const vm = new WwwVm();
window.__wwwvm = vm;

// Forward terminal keystrokes to the guest UART.
term.onData((data) => {
  if (!vm.is_booted()) return;
  const bytes = new TextEncoder().encode(data);
  vm.send_input(bytes);
});

let rafHandle = 0;
const outputListeners = new Set();
function pump() {
  // Each frame: budget for ~50k CPU steps, then read output.
  const steps = vm.run(50_000);
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
  $("diag").textContent =
    `booted=${vm.is_booted()}  halted=${vm.is_halted()}  steps/frame=${steps}`;
  if (vm.is_halted()) {
    setStatus("halted", "");
    return;
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
  if (guestKind === "interactive") {
    vm.load_interactive_demo();
  } else {
    vm.load_default_guest();
  }
  vm.set_autorun(autorun);
  vm.boot();
  setStatus("running", "running");
  pump();
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
