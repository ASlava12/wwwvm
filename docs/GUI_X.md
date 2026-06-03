# Running X (a graphical desktop) in the guest

Status (2026-06-03): **a minimal desktop runs in-guest** — Xorg at 1280×800 on
the linear framebuffer (`/dev/fb0`, the same pixels the host blits to the
`<canvas>`), with the `twm` window manager and an `xterm` terminal (both appear
in `xwininfo -root -tree`). `xsetroot -solid` paints the root window. Keyboard
and mouse input devices (`/dev/input/event0`, `event1`) are present (see the
8042 controller in `crates/devices/src/keyboard.rs`).

This is the reference recipe, validated on the native `alpine_console` example
with the Alpine netboot `vmlinuz-lts`. The browser path follows the same steps
inside its guest.

## Prerequisites (already handled by `alpine_console`)

1. **Framebuffer + DRM/input modules.** Fetch the GUI modules and run with a
   framebuffer:

   ```sh
   scripts/fetch-alpine-assets.sh --with-gui   # simpledrm + evdev + psmouse …
   WWWVM_FB=1280x800 WWWVM_NET_STUB=1 \
     cargo run -p wwwvm-vm --release --example alpine_console
   ```

   `/init` loads `simpledrm` (→ `/dev/dri/card0` **and** `/dev/fb0`), `evdev`,
   `psmouse`, and mounts `/proc`, `/sys`, and `/dev/pts`. **`/sys` is
   essential**: udev (and therefore Xorg) enumerates devices through sysfs —
   without it X aborts with "no screens found" before it even probes the GPU.
   **`/dev/pts` (devpts)** is needed for pseudo-terminals — without it `xterm`
   (and `ssh`/`tmux`/`script`) can't allocate a pty and exits immediately.

2. **Input.** The 8042 controller binds the keyboard (`atkbd` → `event0`) and
   PS/2 mouse (`psmouse` → `event1`); the host injects events via
   `Vm::push_scancode` / `Keyboard::push_mouse_packet`.

## In-guest: one command

If you fetched assets with `--with-gui`, the helper script is already at
`/guest-startx.sh` in the guest — it does everything below (udev, packages,
fbdev config, an auto-placing `twm`, and an `xterm`) in one shot:

```sh
sh /guest-startx.sh
```

(In the browser, click the framebuffer canvas first so it has input focus.) The
manual steps follow, for reference.

## In-guest: install X and start it

```sh
# udev must be running so X can enumerate /dev/fb0 + input devices.
udevd --daemon; udevadm trigger; udevadm settle

# The fbdev driver — NOT modesetting (see "Why fbdev" below).
apk add xorg-server xf86-video-fbdev xf86-input-libinput xrandr xsetroot eudev

mkdir -p /etc/X11/xorg.conf.d
cat > /etc/X11/xorg.conf.d/10-fbdev.conf <<'EOF'
Section "Device"
    Identifier "fb"
    Driver     "fbdev"
    Option     "fbdev" "/dev/fb0"
EndSection
EOF

X :0 vt1 -noreset &
DISPLAY=:0 xrandr                       # → "1280x800" connected
DISPLAY=:0 xsetroot -solid '#1020ff'    # paints the framebuffer blue
```

Then launch a window manager / client on `DISPLAY=:0`:

```sh
apk add twm xterm font-misc-misc
DISPLAY=:0 twm &
DISPLAY=:0 xterm &
```

`xwininfo -root -tree` then lists the `xterm` and `TWM Icon Manager` windows.
(twm defaults to manual mouse placement for new windows, so `xterm` starts 1×1
until placed — set `RandomPlacement` in `~/.twmrc` to auto-place.)

Harmless noise: `(EE) FBDEV(0): FBIOPUTCMAP: Invalid argument` — the fbdev
driver tries to load a palette, which the truecolor (32bpp) framebuffer has no
use for; the guest kernel rejects it and X carries on.

## In the browser

The browser runs the same guest, so the same recipe applies inside it; two extra
pieces wire it to the page:

- **Input.** The terminal feeds the guest UART; a graphical guest instead needs
  PS/2 events. `web/main.js` (+ `web/ps2-keymap.js`) captures keyboard and mouse
  on the framebuffer `<canvas>` and turns them into Set-1 scan codes
  (`push_scancode`) and mouse packets (`push_mouse_packet`). **Click the canvas
  to focus it**, then typing and pointer motion go to the guest. (Mouse motion
  uses relative `movementX/Y`; without pointer lock the guest cursor tracks
  deltas, which is enough to drive a WM.) Rebuild the wasm bundle
  (`wasm-pack …`) so `push_mouse_packet` is exported.

- **Init.** The guest's initramfs `/init` must mount `/proc`, `/sys`,
  `/dev/pts`, load the GUI modules, and start udev + X. The easiest way to get a
  matching initramfs is to dump the one the native example builds (it already
  does the mounts + `--with-gui` module loads):

  ```sh
  WWWVM_FB=1280x800 WWWVM_NET_STUB=1 WWWVM_DUMP_INITRAMFS=/tmp/initramfs.cpio \
    cargo run -p wwwvm-vm --release --example alpine_console
  ```

  then upload that initramfs (and `vmlinuz-lts`) in the web UI, enable the
  framebuffer, boot, and run the apk/udev/X steps above in the guest.

> Status: the input plumbing (canvas → PS/2) is in place but **not yet
> confirmed end-to-end in a real browser** — that needs eyes on the canvas
> (like the networking + Web-Worker milestones were user-confirmed).

## Why `fbdev`, not `modesetting`

The modern `modesetting` driver is the usual choice on a DRM device
(`/dev/dri/card0`, which `simpledrm` provides). It does **not** work on Alpine
**x86 (32-bit)** here: `modesetting_drv.so` (and `libglx.so`) link against
`libgbm.so.1` → `libgallium-<ver>.so`, and **no package on the x86 repo
provides `libgallium-<ver>.so`** (`apk add so:libgallium-<ver>.so` resolves to
nothing; the file never lands on disk). So `modesetting_drv.so` can't even
`dlopen`, and X reports "No drivers available → no screens found".

`xf86-video-fbdev` has none of that chain — it just `mmap`s `/dev/fb0`. It needs
no gbm/gallium/GL, which is exactly right for our simple linear framebuffer, and
its output lands directly in the pixels the host shows on the canvas. If a
future Alpine x86 mesa ships the gallium lib, `modesetting` becomes available as
an (accelerated) alternative.

## Recap of what made it work (each found by in-guest probing)

| Symptom | Cause | Fix |
|---|---|---|
| `i8042: Can't read CTR`, no input | 8042 was a stub | full 8042 controller (commit `a35e6cd`) |
| `/dev/dri/card0` / `/dev/fb0` absent | display drivers are modloop modules | `--with-gui` + `/init` insmod (commit `bf60f9c`) |
| X "no screens found", no probe lines | `/sys` not mounted → udev finds nothing | mount `proc` + `sysfs` in `/init` |
| X "no drivers available" (`libgallium`) | modesetting needs an absent mesa lib | use the `fbdev` driver on `/dev/fb0` |
| `xterm` exits immediately | `/dev/pts` not mounted → no pty | mount `devpts` in `/init` |
