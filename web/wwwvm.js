// wwwvm — high-level browser API over the generated wasm `WwwVm`.
//
// The raw `WwwVm` exposes ~80 low-level methods; this wraps the common path
// (init → boot a real Linux/Alpine kernel → stream serial I/O → optional
// framebuffer → snapshot) behind a small class so embedding is a few lines:
//
//   import { ready, Vm } from "./wwwvm.js";
//   await ready();
//   const [kernel, initrd] = await Promise.all([
//     fetch("images/vmlinuz-lts").then(r => r.arrayBuffer()),
//     fetch("images/alpine-console.cpio.gz").then(r => r.arrayBuffer()),
//   ]);
//   const vm = new Vm({ ramMiB: 256 }).onOutput(s => term.write(s));
//   vm.bootLinux(kernel, initrd);
//   term.onData(d => vm.send(d));   // keystrokes → guest UART
//
// This runs the VM on the MAIN thread (simplest). A slow boot briefly blocks the
// tab; for production run it in a Web Worker — see web/vm-worker.js and
// docs/EMBED.md. Networking (NAT/relay) and multi-VM LANs are advanced and live
// in vm-worker.js / lan.js; this wrapper is the single-VM starting point.

import init, { WwwVm } from "./pkg/wwwvm_wasm.js";

let _ready;
/** Initialise the wasm module once. Await before constructing a Vm. */
export function ready() {
  return (_ready ||= init());
}

const u8 = (b) => (b instanceof Uint8Array ? b : new Uint8Array(b));

function seedClock(vm) {
  if (typeof vm.set_cmos_time !== "function") return;
  const d = new Date(); // seed the RTC with host UTC so the guest's date is right
  vm.set_cmos_time(
    d.getUTCFullYear() % 100, d.getUTCMonth() + 1, d.getUTCDate(),
    d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds(),
  );
}

export class Vm {
  /** @param {{ramMiB?: number, runBudget?: number}} opts */
  constructor({ ramMiB = 256, runBudget = 1_500_000 } = {}) {
    this.raw = WwwVm.new_with_ram_size(ramMiB * 1024 * 1024); // escape hatch for advanced use
    this._budget = runBudget;
    this._running = false;
    this._tick = 0;
    this._out = null;
    this._frame = null;
  }

  /** Register a serial-output callback: fn(text). Returns this (chainable). */
  onOutput(fn) { this._out = fn; return this; }
  /** Register a framebuffer callback: fn({width,height,stride,bytes}). */
  onFrame(fn) { this._frame = fn; return this; }

  /**
   * Boot a real bzImage kernel + optional initramfs (ArrayBuffer|Uint8Array).
   * @param kernel  vmlinuz bzImage
   * @param initrd  cpio(.gz) initramfs, or null
   * @param opts.cmdline      kernel command line
   * @param opts.framebuffer  {width,height} to expose an efifb (else serial only)
   */
  bootLinux(kernel, initrd, { cmdline = "console=ttyS0 panic=10 loglevel=4", framebuffer = null } = {}) {
    seedClock(this.raw);
    const entry = this.raw.load_bzimage(u8(kernel));
    this.raw.set_kernel_cmdline(cmdline);
    if (initrd) this.raw.set_ramdisk(u8(initrd));
    if (framebuffer) this.raw.enable_framebuffer(framebuffer.width, framebuffer.height);
    this.raw.start_protected_mode_at(entry);
    this._loop();
    return this;
  }

  /** Send raw bytes/text to the guest serial console (keystrokes). */
  send(data) {
    this.raw.send_input(typeof data === "string" ? new TextEncoder().encode(data) : u8(data));
  }

  /** Capture a portable snapshot (Uint8Array) — restore later with restore(). */
  snapshot() { return this.raw.snapshot_export(); }
  /** Restore a snapshot taken with snapshot() and resume running. */
  restore(buf) { this.raw.restore_export(u8(buf)); this._loop(); }

  /** Stop the run loop (the VM state is kept; call bootLinux/restore to resume). */
  stop() { this._running = false; }
  /** Whether the guest is idle in HLT (waiting for input/timer). */
  get halted() { return this.raw.is_halted(); }

  _loop() {
    if (this._running) return;
    this._running = true;
    const step = () => {
      if (!this._running) return;
      try {
        // Idle (HLT) → small budget so we don't spin a core; active → full.
        this.raw.run_idle_aware(this.raw.is_halted() ? 250_000 : this._budget);
      } catch (e) {
        this._running = false;
        if (this._out) this._out(`\r\n[cpu error] ${(e && e.message) || e}\r\n`);
        return;
      }
      const out = this.raw.read_output();
      if (out && this._out) this._out(out);
      if (this._frame && ++this._tick % 6 === 0 && this.raw.has_framebuffer && this.raw.has_framebuffer()) {
        this._frame({
          width: this.raw.framebuffer_width(),
          height: this.raw.framebuffer_height(),
          stride: this.raw.framebuffer_stride(),
          bytes: this.raw.framebuffer_bytes(), // 32-bit pixels; see main.js paintFb for channel order
        });
      }
      setTimeout(step, 0);
    };
    setTimeout(step, 0);
  }
}
