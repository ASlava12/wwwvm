# Booting Alpine in the browser

The wasm build can boot a real Linux/Alpine kernel — the `WwwVm` bindings
expose `load_bzimage` / `set_kernel_cmdline` / `set_ramdisk` /
`start_protected_mode_at` and the new `run_idle_aware` stepper (a plain `run`
is HLT-terminal and would stop on Linux's first boot idle-HLT). The web demo
(`web/`) now has a **"Boot Linux / Alpine"** panel wired to all of that on a
fresh 256 MiB VM.

This is the same Vm boot path the native `alpine_console` example uses
(proven to boot Alpine end-to-end); the browser difference is only the wasm
runtime — boot is **slow** (a kernel is hundreds of millions of CPU steps,
and wasm is slower than native), so expect a minute-plus of churn. The page
stays usable; the serial console (`console=ttyS0`) streams into the xterm.

## Steps

1. **Build the wasm bundle** (writes `web/pkg/`):
   ```
   wasm-pack build crates/wasm --target web --out-dir ../../web/pkg --release
   ```

2. **Produce an initramfs cpio** the browser can load (the demo can't pack a
   directory itself). Reuse the native packer's `--dump` path on the extracted
   minirootfs — run it WITHOUT `WWWVM_NET_STUB` so `/init` just drops to a
   shell (the browser has no host network bridge):
   ```
   WWWVM_DUMP_INITRAMFS=/tmp/initramfs.cpio \
   WWWVM_ALPINE_MINIROOT=/tmp/alpine/root \
     cargo run -p wwwvm-vm --release --example alpine_console
   ```
   (You also need a kernel — `vmlinuz-lts`, e.g. `/tmp/wwwvm-alpine/vmlinuz-lts`.)

3. **Serve `web/`** over http and open it:
   ```
   python3 -m http.server -d web 8080
   ```

4. In the **Boot Linux / Alpine** panel: pick the `vmlinuz` as *kernel*, the
   `initramfs.cpio` as *initramfs*, leave the default cmdline, click **Boot
   Linux**, and watch the kernel come up in the terminal.

## What works / doesn't (browser)

- **Boot + text console:** yes (serial → xterm). This is the foundation.
- **Networking:** not yet — the host bridge (`crates/net`) uses OS sockets +
  threads, absent in wasm. The frame API is exposed (`drain_tx_frame` /
  `inject_rx_frame`); the WebSocket-relay-to-proxy is the remaining piece
  (see `docs/BROWSER_NET.md`).
- **Graphics:** **yes — linear framebuffer (efifb) → canvas.** Tick the
  "Graphics framebuffer" box (on by default) before **Boot Linux**: the demo
  calls `vm.enable_framebuffer(w, h)`, which advertises a framebuffer via the
  boot-protocol `screen_info`. The kernel's `efifb` binds to it (no real EFI
  firmware — just the `screen_info` fields), `fbcon` renders the console as
  RGB pixels into a reserved region of guest RAM, and the page reads those
  bytes back (`framebuffer_bytes()`) and blits them onto a `<canvas>`. The
  default cmdline includes `console=tty0` so the VT (and thus fbcon) gets the
  boot log. Alpine's `vmlinuz-lts` has only `efifb` built in (legacy `vesafb`
  was dropped), so the demo uses `VIDEO_TYPE_EFI`. This is text-as-pixels
  (a graphical console); a true GUI (X/Wayland) additionally needs 2D/DRM
  devices the kernel can drive.

Confirm the framebuffer path natively (no browser) against the real Alpine
kernel:

```
WWWVM_FB=800x600 WWWVM_FB_PROBE=1 \
WWWVM_ALPINE_KERNEL=/tmp/wwwvm-alpine/vmlinuz-lts \
WWWVM_ALPINE_MINIROOT=/tmp/alpine/root \
  cargo run -p wwwvm-vm --release --example alpine_console
```

It boots headlessly, waits for fbcon to take over, then prints the efifb log
line and how many framebuffer bytes are non-zero (pixels fbcon drew).

A Web Worker (to move the VM off the UI thread so boot doesn't freeze the tab)
is the next step.
