#!/usr/bin/env bash
# Build prebuilt browser images into web/images/ + a manifest.json that the web
# UI lists in its image picker. Two variants share one kernel + one minirootfs;
# they differ only in /init:
#   - console : serial-only musl shell (no framebuffer).
#   - gui     : advertises a framebuffer and insmods the DRM + input modules so
#               the guest comes up graphical (fbcon on /dev/dri/card0, evdev
#               input). Install Xorg on top with networking enabled.
#
# The initramfs is packed by the alpine_console example via its
# WWWVM_DUMP_INITRAMFS hook (the browser can't pack a directory itself). The
# GUI /init insmods the DRM/input .ko's, which must be staged in the minirootfs
# (fetch-alpine-assets.sh --with-gui) or the inserts are silent no-ops.
#
# Output (web/images/) is .gitignored — it's large binaries. Re-run after asset
# or example changes. Usage: scripts/build-web-images.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/web/images"
KERNEL="${WWWVM_ALPINE_KERNEL:-/tmp/wwwvm-alpine/vmlinuz-lts}"
MINIROOT="${WWWVM_ALPINE_MINIROOT:-/tmp/alpine/root}"

say() { echo "[build-web-images] $*"; }

# 1. Ensure assets: kernel + minirootfs + the GUI .ko modules (simpledrm etc.).
if [ ! -f "$KERNEL" ] || [ ! -x "$MINIROOT/bin/busybox" ] || [ ! -f "$MINIROOT/simpledrm.ko" ]; then
  say "assets missing — fetching (kernel + minirootfs + GUI modules)…"
  "$ROOT/scripts/fetch-alpine-assets.sh" --with-gui
fi

# 2. Build the packer/dumper example.
say "building alpine_console (release)…"
( cd "$ROOT" && cargo build --release -p wwwvm-vm --example alpine_console >/dev/null )
BIN="$ROOT/target/release/examples/alpine_console"

mkdir -p "$OUT"

# 3. Dump the two initramfs variants. WWWVM_FB set → the GUI /init.
say "packing console initramfs…"
WWWVM_ALPINE_KERNEL="$KERNEL" WWWVM_ALPINE_MINIROOT="$MINIROOT" \
  WWWVM_DUMP_INITRAMFS="$OUT/alpine-console.cpio" "$BIN"
say "packing GUI initramfs…"
WWWVM_ALPINE_KERNEL="$KERNEL" WWWVM_ALPINE_MINIROOT="$MINIROOT" WWWVM_FB=1024x768 \
  WWWVM_DUMP_INITRAMFS="$OUT/alpine-gui.cpio" "$BIN"

# 4. Copy the kernel beside them.
cp -f "$KERNEL" "$OUT/vmlinuz-lts"

# 5. Generate the manifest the web UI fetches. Sizes are advisory (shown in the
#    picker so the user knows the download cost). cmdline mirrors what the native
#    example sets per variant (the GUI one adds console=tty0 so fbcon renders).
CONSOLE_CMD="earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4"
GUI_CMD="earlyprintk=ttyS0,115200 console=tty0 console=ttyS0 panic=10 lpj=1000000 loglevel=4"
ksz=$(stat -c%s "$OUT/vmlinuz-lts")
csz=$(stat -c%s "$OUT/alpine-console.cpio")
gsz=$(stat -c%s "$OUT/alpine-gui.cpio")
cat > "$OUT/manifest.json" <<JSON
{
  "images": [
    {
      "id": "alpine-console",
      "name": "Alpine — console (musl shell, serial)",
      "kernel": "vmlinuz-lts",
      "initramfs": "alpine-console.cpio",
      "cmdline": "$CONSOLE_CMD",
      "gui": false,
      "ramMiB": 256,
      "bytes": $((ksz + csz))
    },
    {
      "id": "alpine-gui",
      "name": "Alpine — GUI (framebuffer + DRM/input)",
      "kernel": "vmlinuz-lts",
      "initramfs": "alpine-gui.cpio",
      "cmdline": "$GUI_CMD",
      "gui": true,
      "fbRes": "1024x768",
      "ramMiB": 512,
      "bytes": $((ksz + gsz))
    }
  ]
}
JSON

say "done → $OUT"
ls -la "$OUT"
