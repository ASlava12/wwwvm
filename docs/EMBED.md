# Embedding wwwvm in another project

`wwwvm` is an x86 PC emulator compiled to WebAssembly: it boots a **real
Linux/Alpine kernel in the browser**, no server required. This guide is the
"starting step" for reusing it — booting a single VM from a fresh page. (Multi-VM
LANs, NAT/internet, and snapshots-to-a-store are layered on top; see the end.)

## What you need (3 things)

1. **The wasm bundle** — `web/pkg/` (built by `wasm-pack`). Contains
   `wwwvm_wasm.js` + `wwwvm_wasm_bg.wasm` (+ `.d.ts`). This is the engine.
2. **The wrapper** — `web/wwwvm.js`: a small high-level API over the raw wasm
   `WwwVm` (init, run loop, serial I/O, framebuffer, snapshot). Optional but
   recommended — without it you'd call ~80 low-level methods yourself.
3. **Boot images** — a kernel (`vmlinuz-lts`) + an initramfs
   (`alpine-console.cpio.gz`, …). These are large (kernel ~8 MiB, rootfs a few
   MiB) so they are **hosted as static assets and fetched by URL**, not bundled.

Serve everything over HTTP(S) from one origin. The `.wasm` must be served with
`Content-Type: application/wasm` (for streaming compile) — e.g. on Apache:
`AddType application/wasm .wasm` (see the deploy `.htaccess`).

## Minimal example

A complete, dependency-light page is in [`web/embed-example.html`](../web/embed-example.html).
The whole integration is:

```js
import { ready, Vm } from "./wwwvm.js";

await ready();                                   // init the wasm module once
const [kernel, initrd] = await Promise.all([
  fetch("images/vmlinuz-lts").then(r => r.arrayBuffer()),
  fetch("images/alpine-console.cpio.gz").then(r => r.arrayBuffer()),
]);

const vm = new Vm({ ramMiB: 256 }).onOutput(s => term.write(s)); // serial out → your UI
term.onData(d => vm.send(d));                    // keystrokes → guest UART
vm.bootLinux(kernel, initrd);
```

Use any display you like (xterm.js in the example, or a `<pre>` if you strip
ANSI). The API is framework-agnostic.

## Wrapper API (`web/wwwvm.js`)

| call | purpose |
|------|---------|
| `await ready()` | initialise the wasm module (once, before `new Vm`) |
| `new Vm({ ramMiB?, runBudget? })` | create a VM (default 256 MiB) |
| `.onOutput(fn)` | serial output callback `fn(text)` |
| `.onFrame(fn)` | framebuffer callback `fn({width,height,stride,bytes})` |
| `.bootLinux(kernel, initrd, { cmdline?, framebuffer? })` | boot a bzImage (+ initramfs) |
| `.send(text\|bytes)` | feed the guest serial console |
| `.snapshot()` → `Uint8Array` | portable snapshot |
| `.restore(buf)` | restore a snapshot and resume |
| `.stop()` / `.halted` | pause the run loop / is the guest idle (HLT) |
| `.raw` | the underlying `WwwVm` — escape hatch for the full API |

For a graphical guest, pass `framebuffer: { width, height }` to `bootLinux` and
render `onFrame` bytes (32-bit pixels; channel order as in `web/main.js`
`paintFb`).

## Building the assets

```sh
# wasm bundle → web/pkg/
wasm-pack build crates/wasm --target web --out-dir ../../web/pkg --release

# boot images → web/images/ (needs the Alpine assets; see scripts/fetch-alpine-assets.sh)
scripts/fetch-alpine-assets.sh --with-net      # kernel + minirootfs + NIC/af_packet modules
scripts/build-web-images.sh                    # → alpine-console / -gui / -lan (+ manifest.json)
```

`web/images/manifest.json` lists the available images (`id`, `kernel`,
`initramfs`, `cmdline`, `ramMiB`, `bytes`) — handy for a picker.

## Production note: run it in a Web Worker

The wrapper above runs on the **main thread** (simplest). A slow boot briefly
blocks the tab, and a backgrounded tab throttles the loop. For production, run
the VM in a Web Worker: `web/vm-worker.js` is a ready worker with a `postMessage`
protocol (boot / input / output / framebuffer / snapshot). `web/main.js` is the
reference client.

## Going further (already built, not in the minimal wrapper)

- **Networking (NAT → internet):** an in-wasm smoltcp NAT tunnels guest TCP over
  a WebSocket relay (`crates/proxy`, run separately, behind `wss://` for https
  pages). See `vm-worker.js` (`net_enable`/`net_pump`).
- **Multi-VM virtual LAN:** several worker VMs bridged by an in-page L2 switch
  (`web/l2-switch.js` + `web/lan.js`), with a hybrid LAN+internet mode
  (`net-route.js`). The full lab is `web/lan.html` (embeddable: `?embed=1`).
- **Snapshot store:** content-addressed paged snapshots → `crates/snapstore`.

## License

Dual-licensed MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
