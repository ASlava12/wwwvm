# Running X (a graphical desktop) in the guest

Status (2026-06-03): **Xorg runs in-guest at 1280×800**, rendering to the
linear framebuffer (`/dev/fb0`) — i.e. to the same pixels the host blits to the
`<canvas>`. `xsetroot -solid` paints the root window successfully. Keyboard and
mouse input devices (`/dev/input/event0`, `event1`) are present (see the 8042
controller in `crates/devices/src/keyboard.rs`).

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
   `psmouse`, and mounts `/proc` + `/sys`. **`/sys` is essential**: udev (and
   therefore Xorg) enumerates devices through sysfs — without it X aborts with
   "no screens found" before it even probes the GPU.

2. **Input.** The 8042 controller binds the keyboard (`atkbd` → `event0`) and
   PS/2 mouse (`psmouse` → `event1`); the host injects events via
   `Vm::push_scancode` / `Keyboard::push_mouse_packet`.

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

Then launch a window manager / client on `DISPLAY=:0` (e.g. `apk add ...` a
tiny WM + `xterm`).

Harmless noise: `(EE) FBDEV(0): FBIOPUTCMAP: Invalid argument` — the fbdev
driver tries to load a palette, which the truecolor (32bpp) framebuffer has no
use for; the guest kernel rejects it and X carries on.

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
