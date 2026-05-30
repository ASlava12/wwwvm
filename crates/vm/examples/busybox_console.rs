//! Interactive busybox console — boot a real Linux i386 kernel, run a
//! dynamically-linked glibc `busybox sh`, and bridge YOUR terminal to the
//! guest's UART so you can type commands and watch it react. This is the
//! live, type-into-it counterpart to the (headless, scripted) milestones
//! in `tests/linux_userspace.rs`.
//!
//! Usage (defaults match the milestones):
//!   WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz \
//!   WWWVM_DYN_ROOTFS=/tmp/wwwvm-linux/rootfs \
//!     cargo run -p wwwvm-vm --release --example busybox_console
//!
//! Both env vars default to /tmp/wwwvm-linux/{vmlinuz,rootfs}. The rootfs
//! must contain bin/busybox + lib/{ld-linux.so.2,libc.so.6,libm.so.6,
//! libcrypt.so.1}.
//!
//! What you'll see: ~30–60 s of kernel boot log, then a busybox shell
//! prompt. There is NO PATH set, so run applets as `busybox ls`,
//! `busybox cat /f`, `busybox awk '...'`, etc.; shell builtins
//! (echo, cd, pwd, for/while/if, $((...)) ) work directly. Ctrl-C quits
//! the emulator (exiting the shell makes the kernel panic — it's PID 1).
//!
//! The host terminal is put into raw mode for the session (see `RawMode`)
//! so your keystrokes aren't echoed twice and the shell's cursor-position
//! queries don't leak `^[[…R` onto the prompt; it's restored on exit.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
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
    let kpath =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let root =
        std::env::var("WWWVM_DYN_ROOTFS").unwrap_or_else(|_| "/tmp/wwwvm-linux/rootfs".to_string());

    let read = |p: &str| -> Vec<u8> {
        std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("error: cannot read {p}: {e}");
            std::process::exit(1);
        })
    };
    let kbytes = read(&kpath);
    let busybox = read(&format!("{root}/bin/busybox"));
    let ld = read(&format!("{root}/lib/ld-linux.so.2"));
    let libc = read(&format!("{root}/lib/libc.so.6"));
    let libm = read(&format!("{root}/lib/libm.so.6"));
    let libcrypt = read(&format!("{root}/lib/libcrypt.so.1"));

    let init = build_init_execve_argv(&["busybox", "sh"]);
    let cpio = build_cpio_tree(
        &init,
        &[
            ("bin/busybox", &busybox),
            ("lib/ld-linux.so.2", &ld),
            ("lib/libc.so.6", &libc),
            ("lib/libm.so.6", &libm),
            ("lib/libcrypt.so.1", &libcrypt),
        ],
    );

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline("earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4");
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    eprintln!("[wwwvm] booting Linux + busybox sh… (~30-60s, then a shell)");
    eprintln!(
        "[wwwvm] run applets as `busybox ls` etc.; builtins work directly; Ctrl-C to quit.\n"
    );

    // Put the host terminal in raw mode for the session (restored on drop).
    // Must happen before the stdin reader thread starts so reads come in
    // un-echoed and un-buffered. None if stdin isn't a tty (piped input).
    let _raw = RawMode::new();

    // Feed the user's keystrokes to the guest UART. A background thread does
    // the blocking stdin reads; the main loop polls the channel so it can
    // keep stepping the VM and flushing output without blocking.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break, // EOF (Ctrl-D on its own line)
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
    // Don't forward keystrokes until /init is up: bytes pushed during early
    // boot get eaten before the tty line discipline is in canonical mode.
    // Buffer them until the guest reaches userspace (same trigger the
    // interactive milestone uses), then flush and forward live.
    const READY: &[u8] = b"started with executable stack";
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
                if boot_log.windows(READY.len()).any(|w| w == READY) {
                    ready = true;
                    boot_log = Vec::new();
                    if !pending.is_empty() {
                        vm.send_input(&pending);
                        pending.clear();
                    }
                }
            }
        }
        // Forward keystrokes (buffer until the shell is ready).
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
                // Idled (HLT waiting for input) or just yielded. Sleep a
                // touch when nothing happened to avoid a busy-spin.
                if steps < chunk && out.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// initramfs / init-stub builders — same machinery the linux_userspace tests
// use, kept self-contained so this example is runnable on its own.
// ---------------------------------------------------------------------------

const INIT_LOAD_ADDR: u32 = 0x0804_8000;
const INIT_ENTRY_OFFSET: u32 = 52 + 32;

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

/// Pack `code` + `data_segments` into a tiny i386 ELF32 ET_EXEC (single
/// PT_LOAD, RWX). Entry = first byte of `code`.
fn make_init_elf32(code: &[u8], data_segments: &[&[u8]], pt_flags: u32) -> Vec<u8> {
    const ELF_HEADER_LEN: u32 = 52;
    const PHDR_LEN: u32 = 32;
    const ENTRY_OFFSET: u32 = ELF_HEADER_LEN + PHDR_LEN;
    const LOAD_ADDR: u32 = INIT_LOAD_ADDR;
    let mut body =
        Vec::with_capacity(code.len() + data_segments.iter().map(|s| s.len()).sum::<usize>());
    body.extend_from_slice(code);
    for s in data_segments {
        body.extend_from_slice(s);
    }
    let filesz = ELF_HEADER_LEN + PHDR_LEN + body.len() as u32;

    let mut elf: Vec<u8> = Vec::with_capacity(52);
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1, 0]);
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&3u16.to_le_bytes()); // e_machine = EM_386
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&(LOAD_ADDR + ENTRY_OFFSET).to_le_bytes()); // e_entry
    elf.extend_from_slice(&ELF_HEADER_LEN.to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_shoff
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&(ELF_HEADER_LEN as u16).to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&(PHDR_LEN as u16).to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    let mut phdr: Vec<u8> = Vec::with_capacity(32);
    phdr.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    phdr.extend_from_slice(&0u32.to_le_bytes()); // p_offset
    phdr.extend_from_slice(&LOAD_ADDR.to_le_bytes()); // p_vaddr
    phdr.extend_from_slice(&LOAD_ADDR.to_le_bytes()); // p_paddr
    phdr.extend_from_slice(&filesz.to_le_bytes()); // p_filesz
    phdr.extend_from_slice(&filesz.to_le_bytes()); // p_memsz
    phdr.extend_from_slice(&pt_flags.to_le_bytes()); // p_flags
    phdr.extend_from_slice(&0x1000u32.to_le_bytes()); // p_align

    let mut binary = elf;
    binary.extend_from_slice(&phdr);
    binary.extend_from_slice(&body);
    binary
}

/// A static i386 /init that `execve("/bin/busybox", argv, [])`s; on failure
/// it prints `[EXECVE-FAIL]` and exits 1.
fn build_init_execve_argv(argv: &[&str]) -> Vec<u8> {
    let path: &[u8] = b"/bin/busybox\0";
    let failmsg: &[u8] = b"[EXECVE-FAIL]\n";
    let argv_strs: Vec<Vec<u8>> = argv
        .iter()
        .map(|s| {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            v
        })
        .collect();

    let build_code =
        |path_addr: u32, argv_addr: u32, envp_addr: u32, failmsg_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(64);
            out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11 (execve)
            out.push(0xBB);
            out.extend_from_slice(&path_addr.to_le_bytes());
            out.push(0xB9);
            out.extend_from_slice(&argv_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&envp_addr.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // execve failed: write [EXECVE-FAIL], exit(1).
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&failmsg_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&(failmsg.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(1)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };

    let code_len = build_code(0, 0, 0, 0).len() as u32;
    let base = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET;
    let path_addr = base + code_len;
    let mut addr = path_addr + path.len() as u32;
    let mut str_addrs = Vec::with_capacity(argv_strs.len());
    for s in &argv_strs {
        str_addrs.push(addr);
        addr += s.len() as u32;
    }
    let failmsg_addr = addr;
    addr += failmsg.len() as u32;
    let argv_arr_addr = addr;
    let argv_arr_len = ((argv_strs.len() + 1) * 4) as u32;
    let envp_addr = argv_arr_addr + argv_arr_len;

    let code = build_code(path_addr, argv_arr_addr, envp_addr, failmsg_addr);
    let mut argv_arr = Vec::with_capacity(argv_arr_len as usize);
    for a in &str_addrs {
        argv_arr.extend_from_slice(&a.to_le_bytes());
    }
    argv_arr.extend_from_slice(&0u32.to_le_bytes());
    let envp = 0u32.to_le_bytes();

    let mut segs: Vec<&[u8]> = Vec::with_capacity(argv_strs.len() + 4);
    segs.push(path);
    for s in &argv_strs {
        segs.push(s);
    }
    segs.push(failmsg);
    segs.push(&argv_arr);
    segs.push(&envp);
    make_init_elf32(&code, &segs, 7)
}

/// Assemble the initramfs: /init, /dev (console/null/zero), /proc, then the
/// named files (each parent dir created once).
fn build_cpio_tree(init_binary: &[u8], files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio_entry("dev/null", &[], 0o020_666, 1, 3));
    archive.extend_from_slice(&cpio_entry("dev/zero", &[], 0o020_666, 1, 5));
    archive.extend_from_slice(&cpio_entry("proc", &[], 0o040_755, 0, 0));
    let mut made = std::collections::BTreeSet::new();
    for (path, _) in files {
        if let Some(slash) = path.rfind('/') {
            let dir = &path[..slash];
            if made.insert(dir.to_string()) {
                archive.extend_from_slice(&cpio_entry(dir, &[], 0o040_755, 0, 0));
            }
        }
    }
    for (path, bytes) in files {
        archive.extend_from_slice(&cpio_entry(path, bytes, 0o100_755, 0, 0));
    }
    archive.extend_from_slice(&cpio_entry("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    archive
}
