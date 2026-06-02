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
#   scripts/fetch-alpine-assets.sh --force    # re-download even if present
#
# --with-net pulls modloop-lts and extracts mii.ko + 8139too.ko into the
# rootfs root (where alpine_console's WWWVM_NET_STUB=1 /init `insmod`s them),
# so `apk` over the network works in the guest. Needs `unsquashfs`.
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
FORCE=0
for a in "$@"; do
  case "$a" in
    --with-net) WITH_NET=1 ;;
    --force) FORCE=1 ;;
    -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
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

# --- NIC modules (optional; needed for in-guest apk over the network) ---
if [ "$WITH_NET" = 1 ]; then
  if [ "$FORCE" = 1 ] || [ ! -s "$ROOT/8139too.ko" ]; then
    command -v unsquashfs >/dev/null 2>&1 || {
      echo "[fetch-alpine] --with-net needs unsquashfs (squashfs-tools)" >&2
      exit 3
    }
    say "downloading modloop-lts (NIC modules) — this is large (~150 MiB)…"
    curl -fsSL -o "$ALP/modloop-lts" "$REL/netboot/modloop-lts"
    rm -rf "$ALP/modloop_x"
    # Extract just the two driver modules (paths vary by kernel rev → wildcard).
    unsquashfs -d "$ALP/modloop_x" "$ALP/modloop-lts" \
      "modules/*/kernel/drivers/net/mii.ko" \
      "modules/*/kernel/drivers/net/ethernet/realtek/8139too.ko" >/dev/null
    find "$ALP/modloop_x" -name mii.ko -exec cp {} "$ROOT/mii.ko" \;
    find "$ALP/modloop_x" -name 8139too.ko -exec cp {} "$ROOT/8139too.ko" \;
    rm -f "$ALP/modloop-lts"
    if [ -s "$ROOT/mii.ko" ] && [ -s "$ROOT/8139too.ko" ]; then
      say "NIC modules in place: $ROOT/{mii,8139too}.ko"
    else
      echo "[fetch-alpine] WARNING: NIC modules not found in modloop" >&2
    fi
  else
    say "NIC modules present — skip"
  fi
fi

say "done. kernel=$KERNEL rootfs=$ROOT"
say "run: WWWVM_ALPINE_KERNEL=$KERNEL WWWVM_ALPINE_MINIROOT=$ROOT \\"
say "       cargo run -p wwwvm-vm --release --example alpine_console"
