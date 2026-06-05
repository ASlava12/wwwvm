//! Interactive **Alpine** console — boot the real Alpine `vmlinuz-lts`
//! kernel with the full Alpine minirootfs (musl libc + the PIE busybox and
//! its ~335 applet symlinks + the real `/etc`, `/sbin`, apk, …) as the
//! initramfs, drop into an interactive musl `busybox sh`, and bridge YOUR
//! terminal to the guest UART so you can type Alpine commands and watch it
//! react. This is the live, type-into-it Alpine counterpart of
//! `busybox_console` (which boots the glibc/Tinycore rootfs) and the
//! type-into-it version of `linux_userspace_alpine_interactive_milestone`.
//!
//! Usage (defaults match the Alpine milestones):
//!   WWWVM_ALPINE_KERNEL=/tmp/wwwvm-alpine/vmlinuz-lts \
//!   WWWVM_ALPINE_MINIROOT=/tmp/alpine/root \
//!     cargo run -p wwwvm-vm --release --example alpine_console
//!
//! The minirootfs is the extracted `alpine-minirootfs-*.tar.gz` tree (see
//! the README's Alpine section for the download). Unlike `busybox_console`,
//! Alpine ships a real symlink farm, so applets resolve by bare name once a
//! PATH is set — but `/init` here execs a bare `busybox sh` with no PATH, so
//! prefer `busybox ls`, `busybox cat /etc/alpine-release`, etc.; shell
//! builtins (echo, cd, for/while/if, `$((…))`) work directly. Ctrl-C quits
//! (exiting the shell panics the kernel — the shell is PID 1).
//!
//! The host terminal is put into raw mode for the session (see `RawMode`)
//! so your keystrokes aren't echoed twice and the shell's cursor-position
//! queries don't leak `^[[…R` onto the prompt; it's restored on exit.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Instant;
use wwwvm_net::nat::NatStack;
use wwwvm_net::{Allowlist, DnsForwarder};
use wwwvm_vm::{Stop, Vm};

/// RAII guard that switches the host terminal to raw mode and restores the
/// original settings on drop. Without this the host tty stays in cooked
/// mode: it echoes every keystroke (so input appears twice — once from the
/// host line discipline, once from the guest tty) and it line-buffers, so
/// the guest shell's `ESC[6n` cursor-position query gets answered late and
/// the terminal's `ESC[<row>;<col>R` reply leaks onto the screen as
/// `^[[8;5R`. Raw mode disables host echo + canonical buffering (fixing
/// both) while keeping OPOST so the guest's `\n` output is still formatted.
/// ISIG is disabled so Ctrl-C arrives as a 0x03 byte we handle at the app
/// level (quit) rather than killing us before this guard's Drop can restore
/// the terminal. If stdin isn't a tty (piped input), `new` returns None and
/// nothing is changed.
struct RawMode {
    fd: i32,
    orig: libc::termios,
}

impl RawMode {
    fn new() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return None; // not a tty (e.g. piped input)
            }
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { fd, orig })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

fn main() {
    let kpath = std::env::var("WWWVM_ALPINE_KERNEL")
        .unwrap_or_else(|_| "/tmp/wwwvm-alpine/vmlinuz-lts".to_string());
    let root =
        std::env::var("WWWVM_ALPINE_MINIROOT").unwrap_or_else(|_| "/tmp/alpine/root".to_string());

    let kbytes = std::fs::read(&kpath).unwrap_or_else(|e| {
        eprintln!("error: cannot read kernel {kpath}: {e}");
        std::process::exit(1);
    });

    // /init: print a readiness line we can gate keystroke-forwarding on,
    // then run an interactive shell with JOB CONTROL so Ctrl+C interrupts the
    // foreground program (e.g. `ping`) instead of doing nothing. The shell
    // must be a session leader owning a controlling terminal for the tty layer
    // to deliver SIGINT to the foreground process group; a bare `exec sh` as
    // PID 1 gets no controlling tty ("can't access tty; job control turned
    // off"). So launch it via `setsid -c` (new session + ctty = stdin) on a
    // real tty device, in a respawn loop that keeps PID 1 alive (a plain
    // `exec setsid` would have setsid's parent — PID 1 — exit → kernel panic).
    const READY_LINE: &str = "[wwwvm] alpine shell ready — type away";
    // Kernel pseudo-filesystems. The kernel auto-mounts devtmpfs on /dev
    // (CONFIG_DEVTMPFS_MOUNT) but does NOT auto-mount /proc or /sys, and a
    // lot of userspace breaks without them — notably udev (and therefore
    // Xorg) enumerates devices through /sys, so without sysfs X finds no
    // screens even though /dev/dri/card0 exists. Mount all three early,
    // defensively (`2>/dev/null` if already mounted or unsupported).
    // devpts is needed for pseudo-terminals — without it any pty allocation
    // fails (xterm exits "can't open pseudo-terminal", and so would ssh/tmux/
    // script). /dev/pts must exist as a mountpoint first.
    const BASE_MOUNTS: &str = "mount -t devtmpfs dev /dev 2>/dev/null\n\
         mount -t proc proc /proc 2>/dev/null\n\
         mount -t sysfs sys /sys 2>/dev/null\n\
         mkdir -p /dev/pts 2>/dev/null\n\
         mount -t devpts devpts /dev/pts 2>/dev/null\n";
    // Prefer the concrete /dev/ttyS0 (a real tty that cleanly becomes the
    // controlling terminal); fall back to /dev/console.
    const SHELL_LAUNCH: &str = "TTY=/dev/ttyS0; [ -c \"$TTY\" ] || TTY=/dev/console\n\
         while :; do setsid -c /bin/busybox sh <\"$TTY\" >\"$TTY\" 2>&1; done\n";
    let net_stub = std::env::var_os("WWWVM_NET_STUB").is_some();
    // When the host net stack is on (WWWVM_NET_STUB=1), have /init bring the
    // guest's networking up for you — load the NIC modules, assign the static
    // IP + default route, point the resolver at the gateway, and rewrite the
    // apk repos to http (https needs a CA bundle we don't ship yet). So a
    // plain run just works: `apk update` / `apk add <pkg>` with no manual
    // setup. The insmods are silent no-ops on a rootfs that lacks the .ko
    // files (the standard minirootfs vs the modroot). PATH is exported so
    // applets — and your interactive commands — resolve by bare name.
    // LAN mode (WWWVM_NET_LAN): for parallel VMs wired together by the in-page
    // L2 switch. Each VM reads its own static IP (and optional gateway) from the
    // kernel cmdline so one image serves a whole LAN, e.g. `wwwvm.ip=10.0.0.5/24`.
    // The browser hub gives each worker a distinct `wwwvm.ip=` (and a distinct
    // MAC via set_nic_mac) before booting. When a `wwwvm.gw=` is also passed
    // (hybrid mode: all VMs on 10.0.2.0/24 with the in-wasm NAT as gateway
    // 10.0.2.2), /init also points the resolver at the gateway and rewrites apk
    // to http — so the VMs reach each other AND the outside world over one NIC.
    let net_lan = std::env::var_os("WWWVM_NET_LAN").is_some();
    let net_setup = if net_lan {
        "export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n\
         insmod /mii.ko 2>/dev/null; insmod /8139too.ko 2>/dev/null; insmod /af_packet.ko 2>/dev/null\n\
         ip link set eth0 up 2>/dev/null\n\
         IP=$(cat /proc/cmdline | tr ' ' '\\n' | sed -n 's/^wwwvm.ip=//p')\n\
         [ -n \"$IP\" ] && ip addr add \"$IP\" dev eth0 2>/dev/null\n\
         GW=$(cat /proc/cmdline | tr ' ' '\\n' | sed -n 's/^wwwvm.gw=//p')\n\
         [ -n \"$GW\" ] && { ip route add default via \"$GW\" 2>/dev/null; \
           echo \"nameserver $GW\" > /etc/resolv.conf; \
           sed -i 's,https://,http://,g' /etc/apk/repositories 2>/dev/null; }\n"
    } else if net_stub {
        "export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n\
         insmod /mii.ko 2>/dev/null; insmod /8139too.ko 2>/dev/null; insmod /af_packet.ko 2>/dev/null\n\
         ip link set eth0 up 2>/dev/null\n\
         ip addr add 10.0.2.15/24 dev eth0 2>/dev/null\n\
         ip route add default via 10.0.2.2 2>/dev/null\n\
         echo 'nameserver 10.0.2.2' > /etc/resolv.conf\n\
         sed -i 's,https://,http://,g' /etc/apk/repositories 2>/dev/null\n"
    } else {
        ""
    };
    // When a framebuffer is requested (WWWVM_FB set), load the DRM + input
    // modules so userspace gets real devices: `simpledrm` is what creates
    // /dev/fb0 here (this kernel's sysfb hands the firmware framebuffer to a
    // simple-framebuffer/DRM device, NOT efifb — without simpledrm there is no
    // /dev/fb0 at all), plus /dev/dri/card0; `evdev` exposes /dev/input/event*
    // (keyboard via atkbd/8042, mouse via psmouse). The netboot vmlinuz-lts
    // ships these as modules; stage them with `fetch-alpine-assets.sh
    // --with-gui`. Order = deps before dependents; insmods are silent no-ops
    // when absent.
    let gui_setup = if std::env::var_os("WWWVM_FB").is_some() {
        "for m in i2c-core drm drm_kms_helper drm_shmem_helper simpledrm \
             evdev mousedev psmouse; do insmod /$m.ko 2>/dev/null; done\n"
    } else {
        ""
    };
    // WWWVM_INIT_GUI_SESSION=1 (used by the prebuilt browser GUI image, which
    // ships Xorg+twm+xterm preinstalled): after the DRM/input modules load,
    // bring udev up and launch X (fbdev on /dev/fb0) + twm + xterm in the
    // background, so the guest comes up in a desktop on the framebuffer canvas
    // while the serial shell stays as a console. Needs WWWVM_FB (gui_setup
    // above must have insmod'd simpledrm → /dev/fb0). Font indices + fontconfig
    // cache are (re)built here because the cross-arch rootfs build skips apk
    // scriptlets. No-ops on a rootfs without X (insmod-style silent failure).
    let gui_session = if std::env::var_os("WWWVM_INIT_GUI_SESSION").is_some() {
        "export HOME=/root\n\
         for d in /usr/share/fonts/* /usr/share/fonts/misc; do [ -d \"$d\" ] && \
           { mkfontscale \"$d\" 2>/dev/null; mkfontdir \"$d\" 2>/dev/null; }; done\n\
         fc-cache -f 2>/dev/null\n\
         [ -S /run/udev/control ] || { /sbin/udevd --daemon 2>/dev/null; \
           udevadm trigger 2>/dev/null; udevadm settle 2>/dev/null; }\n\
         mkdir -p /etc/X11/xorg.conf.d /tmp/.X11-unix; chmod 1777 /tmp/.X11-unix 2>/dev/null\n\
         printf 'Section \"Device\"\\n Identifier \"fb\"\\n Driver \"fbdev\"\\n \
Option \"fbdev\" \"/dev/fb0\"\\nEndSection\\n' > /etc/X11/xorg.conf.d/10-fbdev.conf\n\
         printf 'RandomPlacement\\nNoTitleFocus\\n' > /root/.twmrc\n\
         ( X :0 vt1 -noreset -nolisten tcp > /var/log/x.log 2>&1 &\n\
           i=0; while [ ! -e /tmp/.X11-unix/X0 ] && [ \"$i\" -lt 180 ]; do i=$((i+1)); sleep 1; done; sleep 2\n\
           DISPLAY=:0 xsetroot -solid '#30343f' 2>/dev/null\n\
           DISPLAY=:0 twm > /var/log/twm.log 2>&1 &\n\
           DISPLAY=:0 xterm -fn fixed -geometry 100x30+20+20 > /var/log/xterm.log 2>&1 &\n\
           while :; do dd if=/dev/fb0 of=/dev/fb0 conv=notrunc bs=512k 2>/dev/null; \
             sleep 2; done & ) &\n"
    } else {
        ""
    };
    // The cross-built X rootfs is installed with `apk --no-scripts`, so
    // busybox's applet symlinks (/bin/setsid, /bin/mount, /usr/bin/awk, …) were
    // never created — bare applet names don't resolve and /init can't run
    // mount/setsid/etc. Recreate them with `busybox --install -s` as the very
    // first thing (it runs the x86 busybox by absolute path, no symlink needed),
    // then export a PATH. Gated to the X-session image; the minirootfs images
    // already ship the symlinks.
    let bb_install = if std::env::var_os("WWWVM_INIT_GUI_SESSION").is_some() {
        "/bin/busybox --install -s 2>/dev/null\n\
         export PATH=/bin:/sbin:/usr/bin:/usr/sbin\n"
    } else {
        ""
    };
    // Optional tmpfs RAM disk at /mnt/ramdisk, sized via the kernel cmdline
    // (`wwwvm.ramdisk=<MiB>`, set by the web UI). No-op when the param is absent.
    let ramdisk_setup = "RD=$(cat /proc/cmdline | tr ' ' '\\n' | sed -n 's/^wwwvm.ramdisk=//p')\n\
         [ -n \"$RD\" ] && mkdir -p /mnt/ramdisk && mount -t tmpfs -o size=${RD}m tmpfs /mnt/ramdisk 2>/dev/null\n";
    let init = format!(
        "#!/bin/sh\n{bb_install}{BASE_MOUNTS}{ramdisk_setup}{net_setup}{gui_setup}{gui_session}echo '{READY_LINE}'\n{SHELL_LAUNCH}"
    );
    // WWWVM_INITRAMFS_FILE=<path> boots a PREBUILT initramfs (e.g. a dumped or
    // gzipped cpio) as-is instead of packing the directory — handy for
    // validating the exact image the browser serves (gzip included; the kernel
    // decompresses gzip/xz initramfs natively).
    let cpio = if let Some(path) = std::env::var_os("WWWVM_INITRAMFS_FILE") {
        std::fs::read(&path).unwrap_or_else(|e| {
            eprintln!("error: cannot read initramfs {:?}: {e}", path);
            std::process::exit(1);
        })
    } else {
        match build_cpio_from_dir(Path::new(&root), init.as_bytes()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: cannot pack Alpine rootfs {root}: {e}");
                std::process::exit(1);
            }
        }
    };

    // WWWVM_DUMP_INITRAMFS=<path> writes the packed initramfs cpio and exits,
    // instead of booting — for feeding the browser demo (which can't pack a
    // directory itself): pick this file + a vmlinuz in the "Boot Linux" panel.
    // Run WITHOUT WWWVM_NET_STUB so /init just drops to a shell (the browser
    // has no host net bridge).
    if let Some(path) = std::env::var_os("WWWVM_DUMP_INITRAMFS") {
        match std::fs::write(&path, &cpio) {
            Ok(()) => eprintln!(
                "[wwwvm] wrote initramfs cpio ({} KiB) to {}",
                cpio.len() >> 10,
                Path::new(&path).display()
            ),
            Err(e) => {
                eprintln!("error: cannot write initramfs to {:?}: {e}", path);
                std::process::exit(1);
            }
        }
        return;
    }

    // Guest RAM, in MiB, via WWWVM_RAM_MB (default 256). The rootfs is the
    // initramfs (a RAM-backed tmpfs), so installing heavy packages eats into
    // this: running X needs the full xorg-server closure (mesa + llvm pulled
    // via libGL), which overflows 256 MiB — use WWWVM_RAM_MB=1024 for the GUI.
    let ram_mb = std::env::var("WWWVM_RAM_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&m| m >= 64)
        .unwrap_or(256);
    let mut vm = Vm::with_ram_size(ram_mb * 1024 * 1024);
    vm.set_cmos_time_from_host(); // so the guest's `date` is the real time
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    // WWWVM_FB=WxH (e.g. 800x600) advertises a linear framebuffer so the
    // kernel's efifb binds and fbcon renders the console as pixels — the
    // same path the browser demo uses. Adds console=tty0 so the VT (→
    // fbcon) gets the boot log. Off by default (serial-only).
    let fb = std::env::var("WWWVM_FB").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
    });
    // WWWVM_CMDLINE_EXTRA is appended to the kernel cmdline — e.g.
    // `wwwvm.ip=10.0.0.1/24` for the LAN init (WWWVM_NET_LAN), or any other
    // boot param to test. Empty/unset adds nothing.
    let extra = std::env::var("WWWVM_CMDLINE_EXTRA").unwrap_or_default();
    let extra = if extra.trim().is_empty() {
        String::new()
    } else {
        format!(" {}", extra.trim())
    };
    if let Some((w, h)) = fb {
        vm.enable_linear_framebuffer(w, h, wwwvm_vm::VIDEO_TYPE_EFI);
        vm.set_kernel_cmdline(&format!(
            "earlyprintk=ttyS0,115200 console=tty0 console=ttyS0 panic=10 lpj=1000000 loglevel=4{extra}"
        ));
        eprintln!("[wwwvm] framebuffer {w}x{h}x32 enabled (efifb)");
    } else {
        vm.set_kernel_cmdline(&format!(
            "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4{extra}"
        ));
    }
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    // WWWVM_FB_PROBE=1 (with WWWVM_FB) is a non-interactive teeth-check:
    // boot headlessly, wait for fbcon to take over the framebuffer, then
    // report the efifb log line + how many framebuffer bytes are non-zero
    // (proof the kernel rendered pixels), and exit. Lets us confirm
    // Alpine's efifb binds without a browser.
    if fb.is_some() && std::env::var_os("WWWVM_FB_PROBE").is_some() {
        let mut log: Vec<u8> = Vec::new();
        let mut total = 0u64;
        let budget = 8_000_000_000u64;
        while total < budget {
            let (s, _) = vm.run_steps_idle_aware(20_000_000);
            total += s as u64;
            log.extend_from_slice(&vm.drain_output());
            if log.windows(19).any(|w| w == b"frame buffer device") {
                break;
            }
        }
        // Let fbcon paint the accumulated text.
        let _ = vm.run_steps_idle_aware(1_000_000_000);
        let px = vm.framebuffer_bytes().unwrap_or_default();
        let nonzero = px.iter().filter(|&&b| b != 0).count();
        let text = String::from_utf8_lossy(&log);
        let efifb_line = text
            .lines()
            .find(|l| l.contains("efifb") && l.contains("framebuffer at"))
            .unwrap_or("(no efifb 'framebuffer at' line)");
        eprintln!("[wwwvm] FB PROBE after {total} steps:");
        eprintln!("[wwwvm]   {efifb_line}");
        eprintln!(
            "[wwwvm]   framebuffer non-zero bytes: {nonzero} / {} ({}%)",
            px.len(),
            if px.is_empty() {
                0
            } else {
                nonzero * 100 / px.len()
            }
        );
        if nonzero > 5_000 {
            eprintln!("[wwwvm]   RESULT: efifb rendered pixels — graphics works on Alpine ✓");
        } else {
            eprintln!("[wwwvm]   RESULT: framebuffer is (near-)blank — efifb did NOT render ✗");
        }
        return;
    }

    eprintln!(
        "[wwwvm] booting Alpine (vmlinuz-lts + full minirootfs)… (~30-60s, then a musl shell)"
    );
    eprintln!("[wwwvm] no PATH set — run applets as `busybox ls`; builtins work directly; Ctrl-C to quit.\n");

    // Put the host terminal in raw mode for the session (restored on drop).
    // Must happen before the stdin reader thread starts so reads come in
    // un-echoed and un-buffered. None if stdin isn't a tty (piped input).
    let _raw = RawMode::new();

    // Background thread does the blocking stdin reads; the main loop polls
    // the channel so it can keep stepping the VM and flushing output.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // WWWVM_DUMP_TX=1 prints every Ethernet frame the guest's NIC driver
    // transmits (captured by the RTL8139 bus-master TX path) to stderr, so
    // it doesn't corrupt the guest console on stdout. Useful for confirming
    // `ip link set eth0 up` + an ARP actually puts a frame on the wire.
    let dump_tx = std::env::var_os("WWWVM_DUMP_TX").is_some();

    // WWWVM_NET_STUB=1 turns on the host-side networking stack (crates/net):
    // a smoltcp interface that owns the gateway IP and answers the guest's
    // ARP + ICMP (`ping 10.0.2.2`) and serves DNS on UDP/53. Names are
    // pre-resolved here, ONCE, before the VM runs — so a slow getaddrinfo
    // never freezes the single-threaded step loop, and we only ever vend IPs
    // we resolved ourselves for allowlisted names. Configure the mirror with
    // e.g. WWWVM_PROXY_ALLOWLIST='dl-cdn.alpinelinux.org:80'.
    const HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const HOST_IP: [u8; 4] = [10, 0, 2, 2];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
    let mut nat = net_stub.then(|| {
        let allow = Allowlist::from_env();
        let mut fwd = DnsForwarder::new(HOST_IP, HOST_MAC, allow.clone());
        for host in allow.hosts() {
            match (host.as_str(), 0u16).to_socket_addrs() {
                Ok(addrs) => {
                    let v4: Vec<Ipv4Addr> = addrs
                        .filter_map(|sa| match sa.ip() {
                            IpAddr::V4(ip) => Some(ip),
                            IpAddr::V6(_) => None,
                        })
                        .collect();
                    let n = fwd.cache_resolution(&host, &v4);
                    eprintln!("[wwwvm] DNS pre-resolved {host} → {n} A record(s)");
                }
                Err(e) => eprintln!("[wwwvm] DNS: cannot resolve {host}: {e}"),
            }
        }
        NatStack::new(HOST_IP, HOST_MAC, GUEST_IP, fwd)
    });
    let net_start = Instant::now();

    let mut stdout = std::io::stdout();
    // Don't forward keystrokes until /init's interactive shell is up; early
    // bytes get eaten before the tty line discipline is in canonical mode.
    // Buffer them until the readiness line appears, then flush and go live.
    let ready_marker = READY_LINE.as_bytes();
    let mut ready = false;
    let mut boot_log: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    // WWWVM_PS2_TYPE=<text>: press Ctrl-T (0x14) to "type" <text> into the
    // guest as real PS/2 scan codes (the 8042 → atkbd → evdev path), e.g. into
    // an X client. Triggered on demand so you bring up X first; nothing is
    // injected without the hotkey.
    let ps2_type = std::env::var("WWWVM_PS2_TYPE")
        .ok()
        .filter(|s| !s.is_empty());
    if ps2_type.is_some() {
        eprintln!("[wwwvm] WWWVM_PS2_TYPE set — press Ctrl-T to PS/2-type it into the guest");
    }
    'main: loop {
        // While a TCP flow is live, step only until the guest goes idle
        // (blocked waiting for a NIC frame) so we hand it the next RX batch
        // immediately instead of letting it spin its whole budget on the idle
        // HLT — that's what keeps a download from crawling. At all other times
        // (boot, idle shell) use the big idle-aware batch: there, idle HLTs are
        // timer-waits that smoltcp/the PIT wake internally, so returning on
        // every one would make the loop iterate per-step and crawl.
        let net_active = nat
            .as_ref()
            .is_some_and(|n| n.flow_count() > 0 || n.has_egress());
        let (steps, stop) = if net_active {
            vm.run_steps_until_idle(2_000_000)
        } else {
            vm.run_steps_idle_aware(5_000_000)
        };
        // Always drain TX (else the host queue grows unbounded); feed each
        // frame to the host stack.
        for frame in vm.drain_tx_frames() {
            if dump_tx {
                eprint!("\r\n{}\r\n", describe_eth_frame(&frame));
            }
            if let Some(n) = nat.as_mut() {
                n.push_guest_frame(frame);
            }
        }
        // Drive the host stack and inject its replies — done unconditionally
        // each turn (not nested under TX), so host-originated data still flows
        // when the guest is silent. Bounded per turn; on a full RX ring the
        // frame is requeued to the FRONT and retried next turn (preserving
        // order so the guest doesn't see gaps).
        if let Some(n) = nat.as_mut() {
            n.poll(net_start.elapsed().as_millis() as i64);
            // Inject until the RX ring is full (then requeue-to-front and stop)
            // or the egress queue drains. The ring's own back-pressure paces
            // us; the cap is just a runaway guard.
            let mut guard = 1024;
            while guard > 0 {
                let Some(reply) = n.pop_egress() else { break };
                if vm.inject_rx_frame(&reply) {
                    if dump_tx {
                        eprint!("\r\n[wwwvm RX-inject {} bytes → eth0]\r\n", reply.len());
                    }
                    guard -= 1;
                } else {
                    n.requeue_egress_front(reply); // RX ring full — retry next turn
                    break;
                }
            }
        }
        let out = vm.drain_output();
        if !out.is_empty() {
            let _ = stdout.write_all(&out);
            let _ = stdout.flush();
            if !ready {
                boot_log.extend_from_slice(&out);
                if boot_log
                    .windows(ready_marker.len())
                    .any(|w| w == ready_marker)
                {
                    ready = true;
                    boot_log = Vec::new();
                    if !pending.is_empty() {
                        vm.send_input(&pending);
                        pending.clear();
                    }
                }
            }
        }
        while let Ok(bytes) = rx.try_recv() {
            // In raw mode ISIG is off, so Ctrl-C arrives as a 0x03 byte;
            // treat it as "quit" at the app level (the `_raw` guard's Drop
            // restores the terminal on the way out).
            if bytes.contains(&0x03) {
                eprintln!("\r\n[wwwvm] Ctrl-C — bye.");
                break 'main;
            }
            // Ctrl-T (0x14): inject WWWVM_PS2_TYPE as PS/2 scan codes instead
            // of forwarding it to the UART console.
            if bytes.contains(&0x14) {
                if let Some(text) = &ps2_type {
                    vm.type_ascii(text);
                    eprintln!(
                        "\r\n[wwwvm] PS/2-typed {} char(s) (Ctrl-T)",
                        text.chars().count()
                    );
                }
                continue;
            }
            // Ctrl-B (0x02): inject a small PS/2 mouse move + left click, to
            // test/demo the pointer path to a graphical guest (psmouse →
            // /dev/input/event1) without a real mouse.
            if bytes.contains(&0x02) {
                vm.push_mouse_packet(15, -15, true, false, false); // move + press
                vm.push_mouse_packet(0, 0, false, false, false); // release
                eprintln!("\r\n[wwwvm] PS/2 mouse: move + left-click (Ctrl-B)");
                continue;
            }
            // Ctrl-U (0x15): inject the four arrow keys as PS/2 scan codes, to
            // test/demo the EXTENDED-key path (0xE0-prefixed Set-1 codes, what
            // the browser keymap sends for arrows/Home/End/Del) to a graphical
            // guest. Each is 0xE0 + code (make) then 0xE0 + code|0x80 (break).
            if bytes.contains(&0x15) {
                for &code in &[0x48u8, 0x50, 0x4B, 0x4D] {
                    // up, down, left, right
                    vm.push_scancode(0xE0);
                    vm.push_scancode(code);
                    vm.push_scancode(0xE0);
                    vm.push_scancode(code | 0x80);
                }
                eprintln!("\r\n[wwwvm] PS/2: arrow keys ↑↓←→ (Ctrl-U)");
                continue;
            }
            if ready {
                vm.send_input(&bytes);
            } else {
                pending.extend_from_slice(&bytes);
            }
        }
        match stop {
            Stop::Halted => {
                eprintln!("\n[wwwvm] guest halted (kernel panic / init exited). Bye.");
                break;
            }
            Stop::CpuError(e) => {
                eprintln!("\n[wwwvm] CPU error: {e}");
                break;
            }
            Stop::StepBudget => {
                // Sleep only when there's genuinely nothing to do. While a TCP
                // flow is live (or frames are queued to inject) keep looping at
                // full tilt so the transfer doesn't stall.
                let net_active = nat
                    .as_ref()
                    .is_some_and(|n| n.flow_count() > 0 || n.has_egress());
                if steps < 2_000_000 && out.is_empty() && !net_active {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
}

/// One-line summary of a captured Ethernet frame: dst/src MAC, ethertype
/// (decoded for ARP/IPv4/IPv6), length, and a hex dump of the first bytes.
fn describe_eth_frame(f: &[u8]) -> String {
    let mac = |o: usize| {
        (0..6)
            .map(|i| format!("{:02x}", f.get(o + i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(":")
    };
    let ethertype = if f.len() >= 14 {
        ((f[12] as u16) << 8) | f[13] as u16
    } else {
        0
    };
    let proto = match ethertype {
        0x0806 => "ARP",
        0x0800 => "IPv4",
        0x86DD => "IPv6",
        _ => "?",
    };
    let hex: String = f
        .iter()
        .take(42)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[wwwvm TX {} bytes] dst={} src={} type=0x{:04x}({}) | {}{}",
        f.len(),
        mac(0),
        mac(6),
        ethertype,
        proto,
        hex,
        if f.len() > 42 { " …" } else { "" }
    )
}

// ---------------------------------------------------------------------------
// cpio-from-directory packer — mirrors `build_cpio_from_dir` in
// tests/linux_userspace.rs, kept self-contained so this example runs alone.
// ---------------------------------------------------------------------------

/// One newc-format cpio entry (header + name + data, 4-byte aligned).
fn cpio_entry(name: &str, data: &[u8], mode: u32, rdevmaj: u32, rdevmin: u32) -> Vec<u8> {
    cpio_entry_mtime(name, data, mode, rdevmaj, rdevmin, 0)
}

/// Like [`cpio_entry`] but with an explicit mtime (newc field index 5,
/// Unix seconds) so `ls -l` in the guest shows real file dates, not 1970.
fn cpio_entry_mtime(
    name: &str,
    data: &[u8],
    mode: u32,
    rdevmaj: u32,
    rdevmin: u32,
    mtime: u32,
) -> Vec<u8> {
    let namesize = name.len() as u32 + 1;
    let filesize = data.len() as u32;
    let fields = [
        0u32, mode, 0, 0, 1, mtime, filesize, 0, 0, rdevmaj, rdevmin, namesize, 0,
    ];
    let mut hdr = Vec::with_capacity(110);
    hdr.extend_from_slice(b"070701");
    for f in fields {
        hdr.extend_from_slice(format!("{f:08X}").as_bytes());
    }
    hdr.extend_from_slice(name.as_bytes());
    hdr.push(0);
    while hdr.len() & 3 != 0 {
        hdr.push(0);
    }
    let mut out = hdr;
    out.extend_from_slice(data);
    while out.len() & 3 != 0 {
        out.push(0);
    }
    out
}

/// Pack a real on-disk directory tree into a newc cpio initramfs —
/// directories, regular files (with their modes), and symlinks (the busybox
/// applet farm, the musl libc link, …). Injects `/init` (the script we exec)
/// and the /dev nodes the kernel needs (the minirootfs ships an empty /dev).
fn build_cpio_from_dir(root: &Path, init_script: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_script, 0o100_755, 0, 0));

    fn walk(dir: &Path, base: &Path, out: &mut Vec<u8>) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let path = e.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let md = std::fs::symlink_metadata(&path)?;
            let mode = md.permissions().mode() & 0o7777;
            // Carry the file's real host mtime so `ls -l` shows real dates.
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            if md.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                out.extend_from_slice(&cpio_entry_mtime(
                    &rel,
                    target.to_string_lossy().as_bytes(),
                    0o120_000 | 0o777,
                    0,
                    0,
                    mtime,
                ));
            } else if md.is_dir() {
                out.extend_from_slice(&cpio_entry_mtime(&rel, &[], 0o040_000 | mode, 0, 0, mtime));
                walk(&path, base, out)?;
            } else if md.is_file() {
                let data = std::fs::read(&path)?;
                out.extend_from_slice(&cpio_entry_mtime(
                    &rel,
                    &data,
                    0o100_000 | mode,
                    0,
                    0,
                    mtime,
                ));
            }
            // else: device node / FIFO / socket — skip. `fs::read` on one (e.g.
            // a real rootfs's /dev/urandom from alpine-base) would block forever
            // or stream endlessly; the guest's devtmpfs mount plus the essential
            // /dev nodes injected below cover what's needed at boot.
        }
        Ok(())
    }
    walk(root, root, &mut archive)?;

    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio_entry("dev/null", &[], 0o020_666, 1, 3));
    archive.extend_from_slice(&cpio_entry("dev/zero", &[], 0o020_666, 1, 5));
    archive.extend_from_slice(&cpio_entry("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    Ok(archive)
}
