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
BRANCH="${ALPINE_BRANCH:-v3.21}"
ALP="$(dirname "$MINIROOT")"   # /tmp/alpine — docker mounts this
XROOT="$ALP/xroot"             # the X-preinstalled rootfs (cross-built)

# --with-x also builds a heavyweight "Alpine + X desktop" image (Xorg + twm +
# xterm preinstalled → boots straight to a desktop, no in-guest apk). It needs
# docker (the host can't run the x86 apk) and the network. The image is large
# (~130 MiB) and wants ~1 GiB guest RAM.
WITH_X=0
for a in "$@"; do
  case "$a" in
    --with-x) WITH_X=1 ;;
    *) echo "unknown arg: $a (usage: build-web-images.sh [--with-x])" >&2; exit 2 ;;
  esac
done

say() { echo "[build-web-images] $*"; }

# 1. Ensure assets: kernel + minirootfs + the NIC .ko's (for in-guest apk over
#    the browser relay) + the GUI .ko's (simpledrm/evdev/…).
if [ ! -f "$KERNEL" ] || [ ! -x "$MINIROOT/bin/busybox" ] ||
   [ ! -f "$MINIROOT/simpledrm.ko" ] || [ ! -f "$MINIROOT/8139too.ko" ]; then
  say "assets missing — fetching (kernel + minirootfs + NIC + GUI modules)…"
  "$ROOT/scripts/fetch-alpine-assets.sh" --with-net --with-gui
fi

# 2. Build the packer/dumper example.
say "building alpine_console (release)…"
( cd "$ROOT" && cargo build --release -p wwwvm-vm --example alpine_console >/dev/null )
BIN="$ROOT/target/release/examples/alpine_console"

mkdir -p "$OUT"

# 3. Dump the two initramfs variants. WWWVM_NET_STUB=1 → /init insmods the NIC
#    modules, brings eth0 up (10.0.2.15, gw/DNS 10.0.2.2 = the in-wasm NAT) and
#    rewrites apk repos to http — so in-guest `apk` works over the browser relay
#    (tick Networking + run crates/proxy). WWWVM_FB set → the GUI /init also
#    insmods the DRM/input modules.
say "packing console initramfs…"
WWWVM_ALPINE_KERNEL="$KERNEL" WWWVM_ALPINE_MINIROOT="$MINIROOT" WWWVM_NET_STUB=1 \
  WWWVM_DUMP_INITRAMFS="$OUT/alpine-console.cpio" "$BIN"
say "packing GUI initramfs…"
WWWVM_ALPINE_KERNEL="$KERNEL" WWWVM_ALPINE_MINIROOT="$MINIROOT" WWWVM_NET_STUB=1 WWWVM_FB=1024x768 \
  WWWVM_DUMP_INITRAMFS="$OUT/alpine-gui.cpio" "$BIN"

# 4. Copy the kernel beside them.
cp -f "$KERNEL" "$OUT/vmlinuz-lts"

# 4b. (--with-x) Cross-build an X-preinstalled rootfs and pack it. Done in an
#     amd64 Alpine container (the host can't exec the x86 apk): `apk --arch x86
#     --no-scripts` extracts the x86 packages without running x86 scriptlets;
#     we drop the mesa/llvm GL stack (unused by the fbdev driver) + docs to
#     shrink, copy in the NIC/DRM .ko's, and pack with the GUI-session /init
#     (WWWVM_INIT_GUI_SESSION launches X+twm+xterm). Font indices/fontconfig
#     cache (skipped scriptlets) are rebuilt at first boot by /init.
if [ "$WITH_X" = 1 ]; then
  command -v docker >/dev/null || { echo "--with-x needs docker (host can't run x86 apk)"; exit 1; }
  if [ -x "$XROOT/usr/bin/Xorg" ] && [ -z "${WWWVM_REBUILD_XROOT:-}" ]; then
    say "reusing existing X rootfs at $XROOT (set WWWVM_REBUILD_XROOT=1 to rebuild)"
  else
  say "cross-building X rootfs in docker (downloads the X stack — slow)…"
  docker run --rm -v "$ALP:/out" alpine:3.21 sh -c "
    set -e
    rm -rf /out/xroot
    apk --root /out/xroot --arch x86 --initdb -U --allow-untrusted --no-scripts \
      -X http://dl-cdn.alpinelinux.org/alpine/$BRANCH/main \
      -X http://dl-cdn.alpinelinux.org/alpine/$BRANCH/community \
      add alpine-base xorg-server xf86-video-fbdev xf86-input-libinput \
          xrandr xsetroot twm xterm font-misc-misc font-cursor-misc \
          mkfontscale fontconfig eudev >/dev/null
    R=/out/xroot
    rm -rf \$R/usr/lib/libLLVM* \$R/usr/lib/libgallium* \$R/usr/lib/dri \
           \$R/usr/lib/libGLX_mesa* \$R/usr/lib/libEGL* \$R/usr/lib/libgbm* \
           \$R/usr/lib/xorg/modules/extensions/libglx.so \
           \$R/usr/share/man \$R/usr/share/doc \$R/usr/share/locale \
           \$R/usr/share/licenses \$R/usr/share/gtk-doc \$R/usr/lib/*.a \$R/usr/include
    cp /out/root/*.ko /out/xroot/ 2>/dev/null || true
    chmod -R a+rX /out/xroot
  "
  fi
  say "packing X initramfs (this is large)…"
  WWWVM_ALPINE_KERNEL="$KERNEL" WWWVM_ALPINE_MINIROOT="$XROOT" \
    WWWVM_FB=1024x768 WWWVM_NET_STUB=1 WWWVM_INIT_GUI_SESSION=1 \
    WWWVM_DUMP_INITRAMFS="$OUT/alpine-x.cpio" "$BIN"
fi

# 4c. Gzip the initramfs images. The kernel decompresses gzip initramfs natively
#     (it's how Alpine itself boots), so this shrinks the download a lot — the X
#     image especially (~132 → ~50 MiB) — for little extra boot cost. The web UI
#     hands set_ramdisk the gzipped bytes as-is; the guest kernel inflates them.
say "gzipping initramfs images…"
for f in "$OUT"/alpine-*.cpio; do [ -f "$f" ] && gzip -f -9 "$f"; done

# 5. Generate the manifest the web UI fetches. Sizes are advisory (shown in the
#    picker so the user knows the download cost). cmdline mirrors what the native
#    example sets per variant (the GUI ones add console=tty0 so fbcon renders).
CONSOLE_CMD="earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4"
GUI_CMD="earlyprintk=ttyS0,115200 console=tty0 console=ttyS0 panic=10 lpj=1000000 loglevel=4"
ksz=$(stat -c%s "$OUT/vmlinuz-lts")
csz=$(stat -c%s "$OUT/alpine-console.cpio.gz")
gsz=$(stat -c%s "$OUT/alpine-gui.cpio.gz")
IMAGES=$(cat <<JSON
    {
      "id": "alpine-console",
      "name": "Alpine — console (musl shell, serial)",
      "kernel": "vmlinuz-lts",
      "initramfs": "alpine-console.cpio.gz",
      "cmdline": "$CONSOLE_CMD",
      "gui": false,
      "ramMiB": 256,
      "bytes": $((ksz + csz))
    },
    {
      "id": "alpine-gui",
      "name": "Alpine — GUI (framebuffer + DRM/input)",
      "kernel": "vmlinuz-lts",
      "initramfs": "alpine-gui.cpio.gz",
      "cmdline": "$GUI_CMD",
      "gui": true,
      "fbRes": "1024x768",
      "ramMiB": 512,
      "bytes": $((ksz + gsz))
    }
JSON
)
if [ "$WITH_X" = 1 ] && [ -f "$OUT/alpine-x.cpio.gz" ]; then
  xsz=$(stat -c%s "$OUT/alpine-x.cpio.gz")
  IMAGES="$IMAGES,
    {
      \"id\": \"alpine-x\",
      \"name\": \"Alpine — X desktop (Xorg + twm, preinstalled)\",
      \"kernel\": \"vmlinuz-lts\",
      \"initramfs\": \"alpine-x.cpio.gz\",
      \"cmdline\": \"$GUI_CMD\",
      \"gui\": true,
      \"fbRes\": \"1024x768\",
      \"ramMiB\": 1024,
      \"bytes\": $((ksz + xsz))
    }"
fi
printf '{\n  "images": [\n%s\n  ]\n}\n' "$IMAGES" > "$OUT/manifest.json"

say "done → $OUT"
ls -la "$OUT"
