#!/bin/sh
# One-command X desktop bring-up — run this INSIDE the wwwvm Alpine guest.
#
# Prerequisites (provided by booting the alpine_console example with a
# framebuffer + the GUI modules + networking + enough RAM):
#   scripts/fetch-alpine-assets.sh --with-gui --with-net
#   WWWVM_FB=1280x800 WWWVM_NET_STUB=1 WWWVM_RAM_MB=1024 \
#     cargo run -p wwwvm-vm --release --example alpine_console
# i.e. /dev/fb0 + /dev/input/event* exist and apk can reach the network.
# ~1 GiB RAM is needed: the rootfs is a RAM-backed initramfs and xorg-server
# pulls the mesa + llvm GL stack (~200 MiB) even though the fbdev driver
# doesn't use it. 256 MiB overflows and the install half-fails.
#
# Then in the guest:  sh /guest-startx.sh
# In the browser, click the framebuffer canvas first so it has input focus.
#
# Idempotent: re-running re-uses the installed packages and just restarts X.
set -e

export HOME=/root
export PATH=/bin:/sbin:/usr/bin:/usr/sbin

# X + the fbdev driver (NOT modesetting — see docs/GUI_X.md), a tiny WM, a
# terminal, fonts, and udev. We launch X directly (no xinit/startx). apk pulls
# the mesa GL stack as an xorg-server dependency regardless; that's why ~1 GiB
# RAM is needed. `|| true` so a non-fatal hiccup doesn't abort us before the
# binary check below. (apk must come before udevd: eudev provides udevd.)
apk update >/dev/null 2>&1 || true
apk add --no-progress \
    xorg-server xf86-video-fbdev xf86-input-libinput \
    xrandr xsetroot twm xterm font-misc-misc eudev >/dev/null 2>&1 || true

# Confirm the pieces we actually need landed — the usual failure is too little
# RAM (the initramfs tmpfs fills up mid-install).
for bin in X twm xterm udevd; do
    command -v "$bin" >/dev/null 2>&1 || {
        echo "[guest-startx] '$bin' missing after apk — likely out of space." >&2
        echo "[guest-startx] Boot with more RAM: WWWVM_RAM_MB=1024 (see docs/GUI_X.md)." >&2
        exit 1
    }
done

# udev — Xorg enumerates /dev/fb0 and the input devices through it. Without a
# running udevd + a populated db, X aborts "no screens found". The control
# socket marks an already-running daemon (re-run friendly).
if [ ! -S /run/udev/control ]; then
    udevd --daemon
    udevadm trigger
    udevadm settle
fi

# Drive the simple linear framebuffer via /dev/fb0.
mkdir -p /etc/X11/xorg.conf.d
cat > /etc/X11/xorg.conf.d/10-fbdev.conf <<'EOF'
Section "Device"
    Identifier "fb"
    Driver     "fbdev"
    Option     "fbdev" "/dev/fb0"
EndSection
EOF

# twm with RandomPlacement so new windows appear on their own — otherwise twm
# waits for a mouse click to place each window, which looks like a hang.
cat > "$HOME/.twmrc" <<'EOF'
RandomPlacement
NoTitleFocus
RestartPreviousState
Color { BorderColor "slategrey" }
EOF

# Launch X directly (no startx/xinit), then the WM and a terminal on it. This
# script stays alive as their parent and blocks until X exits (a backgrounded
# desktop whose parent exits gets torn down) — like `startx`. Run it in the
# background (`sh /guest-startx.sh &`) if you want your shell back.
echo "[guest-startx] starting X on /dev/fb0 — watch the canvas"
X :0 vt1 -noreset > /var/log/wwwvm-x.log 2>&1 &
xpid=$!
# Wait for the server socket, then give it a moment to actually accept
# connections (the socket appears slightly before the server is ready).
i=0
while [ ! -e /tmp/.X11-unix/X0 ] && [ "$i" -lt 60 ]; do i=$((i + 1)); sleep 1; done
sleep 3
DISPLAY=:0 twm > /var/log/wwwvm-twm.log 2>&1 &
sleep 2
DISPLAY=:0 xterm -geometry 100x32+30+30 -fa fixed > /var/log/wwwvm-xterm.log 2>&1 &
echo "[guest-startx] X up on :0 (twm + xterm). Log: /var/log/wwwvm-x.log"
wait "$xpid"
