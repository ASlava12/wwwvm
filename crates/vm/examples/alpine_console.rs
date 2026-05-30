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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
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
    // then BECOME an interactive shell reading the console (exec, so the
    // shell — not a wrapper — is what waits on read()).
    const READY_LINE: &str = "[wwwvm] alpine shell ready — type away";
    let init = format!("#!/bin/sh\necho '{READY_LINE}'\nexec /bin/busybox sh\n");
    let cpio = match build_cpio_from_dir(Path::new(&root), init.as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot pack Alpine rootfs {root}: {e}");
            std::process::exit(1);
        }
    };

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    vm.set_cmos_time_from_host(); // so the guest's `date` is the real time
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline("earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4");
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

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

    let mut stdout = std::io::stdout();
    let chunk = 5_000_000u32;
    // Don't forward keystrokes until /init's interactive shell is up; early
    // bytes get eaten before the tty line discipline is in canonical mode.
    // Buffer them until the readiness line appears, then flush and go live.
    let ready_marker = READY_LINE.as_bytes();
    let mut ready = false;
    let mut boot_log: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    'main: loop {
        let (steps, stop) = vm.run_steps_idle_aware(chunk);
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
                if steps < chunk && out.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// cpio-from-directory packer — mirrors `build_cpio_from_dir` in
// tests/linux_userspace.rs, kept self-contained so this example runs alone.
// ---------------------------------------------------------------------------

/// One newc-format cpio entry (header + name + data, 4-byte aligned).
fn cpio_entry(name: &str, data: &[u8], mode: u32, rdevmaj: u32, rdevmin: u32) -> Vec<u8> {
    let namesize = name.len() as u32 + 1;
    let filesize = data.len() as u32;
    let fields = [
        0u32, mode, 0, 0, 1, 0, filesize, 0, 0, rdevmaj, rdevmin, namesize, 0,
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
            if md.file_type().is_symlink() {
                let target = std::fs::read_link(&path)?;
                out.extend_from_slice(&cpio_entry(
                    &rel,
                    target.to_string_lossy().as_bytes(),
                    0o120_000 | 0o777,
                    0,
                    0,
                ));
            } else if md.is_dir() {
                out.extend_from_slice(&cpio_entry(&rel, &[], 0o040_000 | mode, 0, 0));
                walk(&path, base, out)?;
            } else {
                let data = std::fs::read(&path)?;
                out.extend_from_slice(&cpio_entry(&rel, &data, 0o100_000 | mode, 0, 0));
            }
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
