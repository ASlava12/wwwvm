//! End-to-end Linux 6.12 i386 boot-to-userspace milestone, captured
//! as a regression test. Mirrors the recipe documented in the
//! README's "Загрузка Linux 6.12" section:
//!
//!     WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz \
//!     cargo test --release -- --ignored linux_userspace_milestone
//!
//! The tests are `#[ignore]` because they depend on a vmlinuz file
//! we don't ship (Tinycore Core ISO `boot/vmlinuz`, 5.85 MB). Each
//! run is ~52 seconds wall-clock — the test bails the moment its
//! marker shows up, vs. the linux_boot example which intentionally
//! runs the full 16 B-step budget for diagnostics and clocks
//! ~10 min. Even at 52 seconds, these aren't in the default sweep.
//!
//! Two milestones live in this file:
//!
//!   - `linux_userspace_milestone` — kernel runs all the way
//!     through `driver_init` + `do_initcalls`, mounts our minimal
//!     initramfs, exec's PID 1 = /init, /init writes "HELLO FROM
//!     USERSPACE\n" via sys_write + THRE IRQ, then exit(42). Two-
//!     stage check pins both ends: HELLO + the kernel panic
//!     `exitcode=0x00002a00` that follows /init's exit.
//!
//!   - `linux_userspace_proc_version_milestone` — wider syscall
//!     surface: /init also mounts procfs (5-arg sys_mount through
//!     int 0x80), opens /proc/version, reads it, prints with a
//!     unique `[USERSPACE /proc/version]:` prefix. Pins mount +
//!     open + read + the kernel-side copy_to_user end-to-end.
//!
//! Both run in parallel under `cargo test -- --ignored`.

use wwwvm_vm::Vm;

/// One cpio newc entry: 6-byte `070701` magic + 13 8-byte ASCII
/// hex fields + name + NUL padded to 4, then the data padded to
/// 4. Shared between every `build_initramfs_*` so a regression in
/// the header layout shows up in *both* milestones at once.
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

/// Pack `code` + `data_segments` into a tiny i386 ELF32 ET_EXEC
/// with a single PT_LOAD whose `p_flags` the caller picks
/// (`5` = R+X for read-only `/init`s, `7` = RWX when the body
/// includes a buf the kernel `copy_to_user`s into). Entry point
/// is right after the ELF header + program header — the first
/// byte of `code`. `data_segments` are concatenated in order
/// after `code`; the caller is responsible for laying out
/// `code` so its absolute references match the segment order.
fn make_init_elf32(code: &[u8], data_segments: &[&[u8]], pt_flags: u32) -> Vec<u8> {
    const ELF_HEADER_LEN: u32 = 52;
    const PHDR_LEN: u32 = 32;
    const ENTRY_OFFSET: u32 = ELF_HEADER_LEN + PHDR_LEN;
    const LOAD_ADDR: u32 = 0x0804_8000;
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
    phdr.extend_from_slice(&pt_flags.to_le_bytes()); // p_flags (5 = R+X, 7 = RWX)
    phdr.extend_from_slice(&0x1000u32.to_le_bytes()); // p_align

    let mut binary = elf;
    binary.extend_from_slice(&phdr);
    binary.extend_from_slice(&body);
    binary
}

/// Load address used by `make_init_elf32`. Both /init variants
/// reference data segments by absolute address, computed from
/// this base + ELF/PHDR header sizes + code length.
const INIT_LOAD_ADDR: u32 = 0x0804_8000;
/// First byte of `code` lives here in the loaded binary's
/// address space — ELF + PHDR sit before it.
const INIT_ENTRY_OFFSET: u32 = 52 + 32;

/// Assemble the cpio archive: /init (regular ELF), /dev (dir),
/// /dev/console (CHR 5:1 so Linux's `console_on_rootfs` can
/// open it), and an optional /proc directory (only the procfs-
/// reader variant needs it as a mount point). Trailing block-
/// alignment to 512 bytes — the kernel's initramfs unpacker
/// stops at TRAILER!!! and ignores the padding, but tools like
/// `cpio -t` get unhappy without it.
fn build_cpio_archive(init_binary: &[u8], proc_dir: bool) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    if proc_dir {
        archive.extend_from_slice(&cpio_entry("proc", &[], 0o040_755, 0, 0));
    }
    archive.extend_from_slice(&cpio_entry("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    archive
}

/// Build the same minimal newc cpio archive the linux_boot example
/// uses for hello mode: /init + /dev + /dev/console (S_IFCHR 5:1).
/// Inlined here so the test stays self-contained (no example
/// dependency from a `tests/` integration file).
fn build_initramfs_hello() -> Vec<u8> {
    let msg: &[u8] = b"HELLO FROM USERSPACE\n";
    let msg_len = msg.len() as u32;

    let build_code = |msg_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(33);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&msg_len.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1 (exit)
        out.extend_from_slice(&[0xBB, 0x2A, 0x00, 0x00, 0x00]); // mov ebx, 42
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let code_len = build_code(0).len() as u32;
    let msg_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let code = build_code(msg_addr);
    let binary = make_init_elf32(&code, &[msg], 5);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_uname` and prints the
/// sysname[0..65] slot bracketed by a unique marker. Last tick's
/// integration test with this builder hung in mid-kernel boot
/// without ever reaching /init exec — see commit message of the
/// next test below for the dump tooling we use to debug it now.
/// The builder itself is straightforward; the bug (if any) is
/// either in the asm encoding or in some kernel-side quirk we
/// haven't characterized yet.
fn build_initramfs_uname() -> Vec<u8> {
    const UTSNAME_LEN: u32 = 390;
    let marker_pre: &[u8] = b"[USERSPACE uname]: ";
    let marker_post: &[u8] = b"\n[USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x7A, 0x00, 0x00, 0x00]); // mov eax, 122
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x41, 0x00, 0x00, 0x00]); // mov edx, 65
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0)
        out.extend_from_slice(&[0xBB, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr);
    let buf_zeros = vec![0u8; UTSNAME_LEN as usize];
    let binary = make_init_elf32(&code, &[marker_pre, marker_post, &buf_zeros], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Sanity check on the cpio builders: each archive starts with
/// the newc magic, so a future refactor of `cpio_entry` or
/// `build_cpio_archive` can't silently produce malformed output.
///
/// When `WWWVM_DUMP_INIT_ARTIFACTS=1` is set, also writes each
/// cpio to `/tmp/wwwvm-{name}.cpio` so an off-line debugger can
/// `cpio -tv` / `cpio -i` / `readelf` them without re-running.
/// The dump is opt-in so the default `cargo test` run doesn't
/// pollute /tmp with files no CI consumer reads.
#[test]
fn init_cpio_archives_start_with_newc_magic() {
    let uname = build_initramfs_uname();
    let proc_version = build_initramfs_proc_version();
    let hello = build_initramfs_hello();
    assert_eq!(&uname[0..6], b"070701");
    assert_eq!(&proc_version[0..6], b"070701");
    assert_eq!(&hello[0..6], b"070701");
    if std::env::var_os("WWWVM_DUMP_INIT_ARTIFACTS").is_some() {
        let _ = std::fs::write("/tmp/wwwvm-uname.cpio", &uname);
        let _ = std::fs::write("/tmp/wwwvm-proc-version.cpio", &proc_version);
        let _ = std::fs::write("/tmp/wwwvm-hello.cpio", &hello);
    }
}

/// Build a cpio whose /init mounts procfs at /proc and reads
/// /proc/version, printing it to stdout bracketed by a unique
/// marker so the test can distinguish it from the kernel's own
/// `Linux version ...` boot banner. The /init binary structure
/// mirrors `build_initramfs_hello`: ELF + PT_LOAD covering code +
/// embedded data + scratch buffer. Wider syscall surface (mount,
/// open, read, write, exit) than hello-mode — the extra syscalls
/// validate that the cross-ring trampoline (int 0x80) handles a
/// 5-argument syscall (mount) end-to-end. The cpio archive also
/// has a /proc directory entry so procfs has a mount point.
fn build_initramfs_proc_version() -> Vec<u8> {
    const BUF_LEN: u32 = 128;
    let marker_pre: &[u8] = b"[USERSPACE /proc/version]: ";
    let marker_post: &[u8] = b"\n[USERSPACE END]\n";
    let proc_str: &[u8] = b"proc\0";
    let slash_proc: &[u8] = b"/proc\0";
    let version_path: &[u8] = b"/proc/version\0";

    let build_code = |marker_pre_addr: u32,
                      marker_post_addr: u32,
                      proc_addr: u32,
                      slash_proc_addr: u32,
                      version_path_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // write(1, marker_pre, marker_pre.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_mount("proc", "/proc", "proc", 0, 0) — sys 21
        out.extend_from_slice(&[0xB8, 0x15, 0x00, 0x00, 0x00]); // mov eax, 21
        out.push(0xBB);
        out.extend_from_slice(&proc_addr.to_le_bytes()); // ebx = source
        out.push(0xB9);
        out.extend_from_slice(&slash_proc_addr.to_le_bytes()); // ecx = target
        out.push(0xBA);
        out.extend_from_slice(&proc_addr.to_le_bytes()); // edx = fstype
        out.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi
        out.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_open(version_path, O_RDONLY, 0) — sys 5
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&version_path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ebx, eax — save fd
        out.extend_from_slice(&[0x89, 0xC3]);
        // sys_read(fd, buf, BUF_LEN) — sys 3
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&BUF_LEN.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov edx, eax — save nbytes
        out.extend_from_slice(&[0x89, 0xC2]);
        // write(1, buf, nbytes)
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, marker_post.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0) — sys 1
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let proc_addr = marker_post_addr + marker_post.len() as u32;
    let slash_proc_addr = proc_addr + proc_str.len() as u32;
    let version_path_addr = slash_proc_addr + slash_proc.len() as u32;
    let buf_addr = version_path_addr + version_path.len() as u32;
    let code = build_code(
        marker_pre_addr,
        marker_post_addr,
        proc_addr,
        slash_proc_addr,
        version_path_addr,
        buf_addr,
    );
    let buf_zeros = vec![0u8; BUF_LEN as usize];
    let binary = make_init_elf32(
        &code,
        &[
            marker_pre,
            marker_post,
            proc_str,
            slash_proc,
            version_path,
            &buf_zeros,
        ],
        7, // R | W | X — kernel copy_to_user writes into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ true)
}

/// Pretty-print a marker-search failure: dump the *full* UART
/// stream to a stable path under `/tmp` (the same directory the
/// vmlinuz already lives in) so a debugger can grep it without
/// re-running, and inline the last 4 KiB into the panic message
/// for at-a-glance triage. 2 KiB turned out to be too small for
/// debugging the `uname` /init attempt — the kernel printed
/// 8 KiB+ of SCSI/PATA probe traffic between the last useful
/// marker and the budget expiry. 4 KiB inline + full file dump
/// covers both shallow and deep diagnosis without spamming the
/// terminal indefinitely.
fn dump_uart_on_failure(cumulative: &[u8], slug: &str) -> String {
    let path = format!("/tmp/wwwvm-userspace-{slug}-failure.bin");
    let footer = match std::fs::write(&path, cumulative) {
        Ok(()) => format!(" (full {} bytes also dumped to {})", cumulative.len(), path),
        Err(_) => String::new(),
    };
    let tail_start = cumulative.len().saturating_sub(4096);
    format!(
        "last 4 KiB of UART output{}:\n{}",
        footer,
        String::from_utf8_lossy(&cumulative[tail_start..])
    )
}

/// Drive the boot for up to `step_budget` instructions, draining
/// UART output in 100M-step chunks, appending it to `cumulative`,
/// and looking for `needle` anywhere in the cumulative output.
/// Returns Ok(step_count_at_hit) or Err(()) on budget exhaustion;
/// `cumulative` is updated either way so the caller can inspect
/// or continue searching for a later marker.
fn run_until_marker(
    vm: &mut Vm,
    needle: &[u8],
    step_budget: u64,
    cumulative: &mut Vec<u8>,
) -> Result<u64, ()> {
    // The caller may have already drained the bytes we want into
    // `cumulative` on a previous run_until_marker pass — e.g. stage
    // 1 drained the chunk containing both HELLO and the panic line,
    // returned at HELLO, and stage 2's needle is now sitting in
    // cumulative before we step a single instruction. Check first
    // before entering the chunk loop.
    if cumulative.windows(needle.len()).any(|w| w == needle) {
        return Ok(0);
    }
    let chunk = 10_000_000u32;
    let mut steps = 0u64;
    while steps < step_budget {
        let (s, _) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        if steps % 100_000_000 < chunk as u64 {
            let out = vm.drain_output();
            if !out.is_empty() {
                cumulative.extend_from_slice(&out);
                if cumulative.windows(needle.len()).any(|w| w == needle) {
                    return Ok(steps);
                }
            }
        }
    }
    Err(())
}

/// Full Linux 6.12 boot to userspace. Skipped if the kernel file
/// isn't present (so contributors without the binary can still
/// run `cargo test -- --ignored`).
///
/// Two-stage check: first wait for `HELLO FROM USERSPACE`
/// (validates write-syscall + THRE path), then keep stepping past
/// /init's `exit(42)` until the kernel panics with the matching
/// exitcode (validates sys_exit + the kernel's panic-on-init-exit
/// shutdown sequence). The second stage adds maybe 30s wall-clock
/// but pins the *full* documented end-to-end chain from the README
/// instead of trusting that "if HELLO works, exit must too."
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_milestone() {
    let path =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {path}: {e}");
            return;
        }
    };

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&bytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let cpio = build_initramfs_hello();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut cumulative = Vec::<u8>::new();
    let hello_steps = run_until_marker(
        &mut vm,
        b"HELLO FROM USERSPACE",
        16_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "HELLO FROM USERSPACE not seen in 16 B steps; {}",
            dump_uart_on_failure(&cumulative, "hello")
        )
    });
    eprintln!("HELLO FROM USERSPACE found after {hello_steps} steps");

    // /init has exited; keep stepping until the kernel completes
    // do_exit cleanup + panic-on-init-exit and prints the exitcode
    // line. exit(42) → exitcode=0x00002a00 (42 << 8). A 4 B step
    // budget is ~3 seconds of guest time at our throughput; the
    // panic message typically lands within a few hundred M steps
    // after HELLO.
    let panic_steps = run_until_marker(
        &mut vm,
        b"exitcode=0x00002a00",
        4_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "kernel panic line `exitcode=0x00002a00` not seen in \
             +4 B steps after HELLO; {}",
            dump_uart_on_failure(&cumulative, "panic-exitcode")
        )
    });
    eprintln!("panic exit code seen after {panic_steps} additional steps");
}

/// Wider-syscall-surface milestone: /init mounts procfs, opens
/// /proc/version, reads it, and prints it bracketed by markers.
/// Pins five distinct syscalls — mount (5-arg!), open, read,
/// write, exit — across the cross-ring trampoline. The kernel
/// itself prints `Linux version 6.12...` early in boot, so the
/// userspace search uses the `[USERSPACE /proc/version]:`
/// prefix to disambiguate (only /init writes that string).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_proc_version_milestone() {
    let path =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {path}: {e}");
            return;
        }
    };

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&bytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let cpio = build_initramfs_proc_version();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(
        &mut vm,
        b"[USERSPACE /proc/version]: Linux version",
        16_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "[USERSPACE /proc/version]: Linux version` marker not seen in 16 B steps; {}",
            dump_uart_on_failure(&cumulative, "proc-version")
        )
    });
    eprintln!("/proc/version userspace read seen after {steps} steps");
}
