#!/bin/sh
# Fetch the Alpine test assets the native boot example/tests expect.
#
# The boot examples (crates/vm/examples/alpine_console.rs) and the Alpine
# milestones default to a kernel at /tmp/wwwvm-alpine/vmlinuz-lts and an
# extracted minirootfs at /tmp/alpine/root. Those live under /tmp, which gets
# reaped between sessions, so re-create them with one command instead of by
# hand:
#
#   scripts/fetch-alpine-assets.sh            # kernel + minirootfs (no network)
#   scripts/fetch-alpine-assets.sh --with-net # also the RTL8139 NIC .ko's
#   scripts/fetch-alpine-assets.sh --with-gui # also the simpledrm/evdev .ko's
#   scripts/fetch-alpine-assets.sh --force    # re-download even if present
#
# --with-net pulls modloop-lts and extracts mii.ko + 8139too.ko into the
# rootfs root (where alpine_console's WWWVM_NET_STUB=1 /init `insmod`s them),
# so `apk` over the network works in the guest. Needs `unsquashfs`.
#
# --with-gui pulls the same modloop-lts and extracts the framebuffer/DRM +
# input module closure (i2c-core, drm, drm_kms_helper, drm_shmem_helper,
# simpledrm, evdev, mousedev, psmouse). alpine_console's /init `insmod`s
# them when present, so `simpledrm` binds the EFI framebuffer → /dev/dri/card0
# (the X `modesetting` device) and `evdev` exposes /dev/input/event*. The
# netboot vmlinuz-lts has these as modules, not built-ins — so without this
# flag there is no userspace graphics/input device. Needs `unsquashfs`.
#
# Override the version/arch/mirror via env: ALPINE_VER, ALPINE_BRANCH,
# ALPINE_ARCH, ALPINE_MIRROR.
set -eu

VER="${ALPINE_VER:-3.21.7}"
BRANCH="${ALPINE_BRANCH:-v3.21}"
ARCH="${ALPINE_ARCH:-x86}"
MIRROR="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"
REL="$MIRROR/$BRANCH/releases/$ARCH"

KDIR=/tmp/wwwvm-alpine
KERNEL="$KDIR/vmlinuz-lts"
ALP=/tmp/alpine
ROOT="$ALP/root"

WITH_NET=0
WITH_GUI=0
FORCE=0
for a in "$@"; do
  case "$a" in
    --with-net) WITH_NET=1 ;;
    --with-gui) WITH_GUI=1 ;;
    --force) FORCE=1 ;;
    -h | --help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done

say() { printf '[fetch-alpine] %s\n' "$*"; }

mkdir -p "$KDIR" "$ROOT"

# --- kernel (standalone netboot file, ~8 MiB) ---
if [ "$FORCE" = 1 ] || [ ! -s "$KERNEL" ]; then
  say "downloading vmlinuz-lts → $KERNEL"
  curl -fsSL -o "$KERNEL" "$REL/netboot/vmlinuz-lts"
else
  say "kernel present ($(wc -c < "$KERNEL") bytes) — skip (--force to redownload)"
fi

# --- minirootfs (~3 MiB tgz) ---
if [ "$FORCE" = 1 ] || [ ! -x "$ROOT/bin/busybox" ]; then
  TGZ="alpine-minirootfs-$VER-$ARCH.tar.gz"
  say "downloading + extracting $TGZ → $ROOT"
  rm -rf "$ROOT"
  mkdir -p "$ROOT"
  curl -fsSL -o "$ALP/$TGZ" "$REL/$TGZ"
  tar -xzf "$ALP/$TGZ" -C "$ROOT"
  rm -f "$ALP/$TGZ"
else
  say "minirootfs present (bin/busybox) — skip (--force to re-extract)"
fi

# Download modloop-lts once (shared by --with-net and --with-gui). The driver
# modules live in this ~150 MiB squashfs; we extract just the .ko's we need.
MODLOOP="$ALP/modloop-lts"
ensure_modloop() {
  command -v unsquashfs >/dev/null 2>&1 || {
    echo "[fetch-alpine] module extraction needs unsquashfs (squashfs-tools)" >&2
    exit 3
  }
  [ -s "$MODLOOP" ] && return 0
  say "downloading modloop-lts (driver modules) — this is large (~150 MiB)…"
  curl -fsSL -o "$MODLOOP" "$REL/netboot/modloop-lts"
}

# Extract one module (by basename, path-wildcarded) from the modloop into ROOT.
# $1 = modloop path glob under modules/*/kernel/...  $2 = destination basename
extract_mod() {
  rm -rf "$ALP/modloop_x"
  unsquashfs -d "$ALP/modloop_x" "$MODLOOP" "modules/*/$1" >/dev/null 2>&1 || true
  found=$(find "$ALP/modloop_x" -name "$2" 2>/dev/null | head -1)
  if [ -n "$found" ]; then
    cp "$found" "$ROOT/$2"
  else
    echo "[fetch-alpine] WARNING: $2 not found in modloop ($1)" >&2
  fi
}

# --- NIC modules (optional; needed for in-guest apk over the network) ---
if [ "$WITH_NET" = 1 ]; then
  if [ "$FORCE" = 1 ] || [ ! -s "$ROOT/8139too.ko" ]; then
    ensure_modloop
    extract_mod "kernel/drivers/net/mii.ko" mii.ko
    extract_mod "kernel/drivers/net/ethernet/realtek/8139too.ko" 8139too.ko
    if [ -s "$ROOT/mii.ko" ] && [ -s "$ROOT/8139too.ko" ]; then
      say "NIC modules in place: $ROOT/{mii,8139too}.ko"
    fi
  else
    say "NIC modules present — skip"
  fi
fi

# --- GUI modules (optional; framebuffer/DRM + input → /dev/dri/card0 + evdev) ---
# Load order matters in /init: i2c-core, drm, drm_kms_helper, drm_shmem_helper,
# simpledrm, evdev, mousedev, psmouse (deps before dependents).
if [ "$WITH_GUI" = 1 ]; then
  if [ "$FORCE" = 1 ] || [ ! -s "$ROOT/simpledrm.ko" ]; then
    ensure_modloop
    extract_mod "kernel/drivers/i2c/i2c-core.ko"            i2c-core.ko
    extract_mod "kernel/drivers/gpu/drm/drm.ko"             drm.ko
    extract_mod "kernel/drivers/gpu/drm/drm_kms_helper.ko"  drm_kms_helper.ko
    extract_mod "kernel/drivers/gpu/drm/drm_shmem_helper.ko" drm_shmem_helper.ko
    extract_mod "kernel/drivers/gpu/drm/tiny/simpledrm.ko"  simpledrm.ko
    extract_mod "kernel/drivers/input/evdev.ko"             evdev.ko
    extract_mod "kernel/drivers/input/mousedev.ko"          mousedev.ko
    extract_mod "kernel/drivers/input/mouse/psmouse.ko"     psmouse.ko
    if [ -s "$ROOT/simpledrm.ko" ] && [ -s "$ROOT/evdev.ko" ]; then
      say "GUI modules in place: simpledrm/evdev/psmouse (+drm deps) → $ROOT"
    fi
  else
    say "GUI modules present — skip"
  fi
  # Stage the one-command X bring-up helper into the guest root (→ lands at
  # /guest-startx.sh in the guest; run `sh /guest-startx.sh`).
  if [ -f "$(dirname "$0")/guest-startx.sh" ]; then
    cp "$(dirname "$0")/guest-startx.sh" "$ROOT/guest-startx.sh"
    chmod +x "$ROOT/guest-startx.sh"
    say "X helper staged: $ROOT/guest-startx.sh (run 'sh /guest-startx.sh' in-guest)"
  fi
fi

rm -rf "$MODLOOP" "$ALP/modloop_x"

say "done. kernel=$KERNEL rootfs=$ROOT"
say "run: WWWVM_ALPINE_KERNEL=$KERNEL WWWVM_ALPINE_MINIROOT=$ROOT \\"
say "       cargo run -p wwwvm-vm --release --example alpine_console"
