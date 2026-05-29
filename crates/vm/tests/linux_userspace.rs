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
//! Production milestones (always passing when run with `--ignored`):
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
//!   - `linux_userspace_time_milestone` — clock primitive:
//!     /init calls `sys_time(NULL)` (syscall 13), writes the
//!     returned 32-bit time_t bracketed by `[USERSPACE TIME=]`
//!     markers, and the test decodes the 4 bytes from cumulative
//!     UART output. Pins CMOS RTC read → kernel timekeeping →
//!     sys_time → cross-ring trampoline → userspace decode.
//!
//!   - `linux_userspace_getpid_milestone` — task-struct
//!     primitive: /init calls `sys_getpid` (syscall 20), writes
//!     the returned pid bracketed by `[USERSPACE PID=]` markers.
//!     Asserts exactly `pid == 1` (any other value would mean
//!     the kernel exec'd /init under a different task struct).
//!
//!   - `linux_userspace_gettimeofday_milestone` — sub-second
//!     clock + struct-to-userspace: /init calls
//!     `sys_gettimeofday(&tv, NULL)` (syscall 78), kernel
//!     copy_to_user's the 8-byte `struct timeval`, /init writes
//!     it bracketed by `[USERSPACE TV=]` markers. Test asserts
//!     `tv_sec ∈ [2020-01-01, Y2038)` and `tv_usec < 1_000_000`.
//!     Different mechanism than the time milestone — that one
//!     proves the syscall ABI moves a u32 through eax, this one
//!     proves the kernel's copy_to_user fills a user-side struct.
//!
//! Bisection probes (also `#[ignore]`, used to characterize the
//! /init-binary-size {600, 602} stall — see
//! `build_initramfs_hello_padded_to` doc-block for the table):
//!
//!   - `linux_userspace_hello_padded_to_600_milestone` (hangs)
//!   - `linux_userspace_hello_padded_to_601_milestone` (passes)
//!   - `linux_userspace_hello_padded_to_608_milestone` (passes)
//!
//! The production milestones run in parallel under
//! `cargo test -- --ignored`. The probes can be re-run
//! individually by name as evidence for the bisection result.

use wwwvm_vm::Vm;

/// One cpio newc entry: 6-byte `070701` magic + 13 8-byte ASCII
/// hex fields + name + NUL padded to 4, then the data padded to
/// 4. Shared between every `build_initramfs_*` so a regression in
/// the header layout shows up across the whole test file at once.
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
    // Heads-up for future contributors: /init binaries of size
    // 600..=602 hit a kernel boot stall — see
    // `build_initramfs_hello_padded_to_600`'s doc-block for the
    // bisection summary. If a new builder accidentally lands a
    // binary in that range, the matching ignored milestone will
    // hang ~9 minutes when run with --ignored. The bug isn't in
    // this helper, but the helper is the chokepoint everyone goes
    // through, so the warning lives here.
    if matches!(init_binary.len(), 600 | 602) && cfg!(debug_assertions) {
        eprintln!(
            "build_cpio_archive: WARNING — /init binary length {} is a known-bad \
             size. The bad set after 17 probes is exactly {{600, 602}} — sparse \
             and non-modular (601 works, 604 works, 608 works, only 600 and 602 \
             hang). Boot stalls in pata_legacy probe — see \
             build_initramfs_hello_padded_to_600.",
            init_binary.len()
        );
    }
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
/// sysname[0..65] slot bracketed by a unique marker. **This
/// builder hangs kernel boot.** Kept around because it's used by
/// `init_cpio_archives_start_with_newc_magic` as a sanity-check
/// input, AND it's a canonical reproducer for the boot stall.
///
/// Bisection result (17-probe sweep `d22718e`..`74dedf3`):
///
///   | /init size | mod 8 | works? |
///   |------------|-------|--------|
///   | 588        |   4   |  ✓     |
///   | 600        |   0   |  ✗     |
///   | 601        |   1   |  ✓     |
///   | 602        |   2   |  ✗     |
///   | 604        |   4   |  ✓     |
///   | 605        |   5   |  ✓     |
///   | 608        |   0   |  ✓     |
///
/// **The bad SET is just {600, 602} — sparse, NOT modular.**
/// The mod-8 hypothesis (`74dedf3`) was refuted by /init=608
/// (mod 8 = 0, same as 600) passing. So something genuinely
/// specific to those two adjacent-ish sizes, not a 3-bit
/// pattern. This builder lands at exactly 600.
///
/// Binary-size math:
///   ELF+PHDR (84) + code (90, 5 syscalls × ~22 bytes) +
///   marker_pre (19) + marker_post (17) + buf_zeros (390) = 600.
///
/// The exact mechanism is still unknown — kernel never reaches
/// /init exec, stuck in pata_legacy probe at 16 B steps. Future
/// debug pass needs host-side cpu.step instrumentation to find
/// where execution diverges from the working hello /init. A
/// faster reproducer would be hello /init padded to exactly 600
/// bytes (84 + 34 + 21 + 461 = 600), avoiding the buf_zeros and
/// the extra syscalls — but writing it requires re-running a
/// 9-minute integration test, which the next debug pass can do.
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
    let time = build_initramfs_time();
    let getpid = build_initramfs_getpid();
    let gettimeofday = build_initramfs_gettimeofday();
    assert_eq!(&uname[0..6], b"070701");
    assert_eq!(&proc_version[0..6], b"070701");
    assert_eq!(&hello[0..6], b"070701");
    assert_eq!(&time[0..6], b"070701");
    assert_eq!(&getpid[0..6], b"070701");
    assert_eq!(&gettimeofday[0..6], b"070701");
    if std::env::var_os("WWWVM_DUMP_INIT_ARTIFACTS").is_some() {
        let _ = std::fs::write("/tmp/wwwvm-uname.cpio", &uname);
        let _ = std::fs::write("/tmp/wwwvm-proc-version.cpio", &proc_version);
        let _ = std::fs::write("/tmp/wwwvm-hello.cpio", &hello);
        let _ = std::fs::write("/tmp/wwwvm-time.cpio", &time);
        let _ = std::fs::write("/tmp/wwwvm-getpid.cpio", &getpid);
        let _ = std::fs::write("/tmp/wwwvm-gettimeofday.cpio", &gettimeofday);
        // Bisection-debug cpios for the {600, 602} stall — the
        // failing minimal reproducer and the just-above-bad-set
        // counter-example. Pair these with
        // `WWWVM_DUMP_REGIONS=1` linux_boot runs to diff the
        // per-EIP-region step histograms and find where the
        // failing run diverges:
        //
        //   WWWVM_INITRD=/tmp/wwwvm-padded-600.cpio \
        //   WWWVM_DUMP_REGIONS=1 cargo run --release --example \
        //   linux_boot -p wwwvm-vm > /tmp/run-600.log
        //
        //   (same with padded-601 → /tmp/run-601.log)
        //
        //   diff <(grep "..  " /tmp/run-600.log) \
        //        <(grep "..  " /tmp/run-601.log)
        let _ = std::fs::write(
            "/tmp/wwwvm-padded-600.cpio",
            build_initramfs_hello_padded_to(600),
        );
        let _ = std::fs::write(
            "/tmp/wwwvm-padded-601.cpio",
            build_initramfs_hello_padded_to(601),
        );
    }
}

/// Hello /init padded to exactly 600 bytes — the **minimal**
/// known reproducer of the binary-size-in-bad-range stall.
/// Confirmed in `4fe22d2`: this /init hangs (519.36 s) the same
/// way the original 600-byte uname /init does, with the kernel
/// stuck in pata_legacy probe at 16 B steps. Binary size 600 is
/// the trigger; no other property required.
///
/// Anatomy:
///   ELF+PHDR (84) + code (34, just write+exit) + msg (21) +
///   461 zero pad = 600.
///
/// For the future kernel-side debug pass: this is the simplest
/// /init that exhibits the bug — anyone instrumenting the
/// kernel can use it instead of the larger uname /init.
fn build_initramfs_hello_padded_to_600() -> Vec<u8> {
    build_initramfs_hello_padded_to(600)
}

/// Tunable variant: hello asm + msg + zero pad to `target_size`
/// bytes total. Used by `build_initramfs_hello_padded_to_600`
/// (the minimal failing reproducer) and could be used by future
/// boundary-probes (target_size=596 to test just below the bad
/// range, target_size=608 to test just above, etc).
fn build_initramfs_hello_padded_to(target_size: usize) -> Vec<u8> {
    let msg: &[u8] = b"HELLO FROM USERSPACE\n";
    let msg_len = msg.len() as u32;
    let build_code = |msg_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(34);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&msg_len.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x2A, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let code_len = build_code(0).len() as u32;
    let msg_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let code = build_code(msg_addr);
    let pad_len = target_size - 84 - code_len as usize - msg_len as usize;
    let pad = vec![0u8; pad_len];
    let binary = make_init_elf32(&code, &[msg, &pad], 5);
    assert_eq!(
        binary.len(),
        target_size,
        "should land at exactly {target_size} bytes"
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Quick unit test: the tunable padded-hello builder produces
/// binaries of the requested size, decoded from the cpio header.
/// Pinned across three points: 596 (just below the bad range),
/// 600 (the minimal failing reproducer), 604 (just above).
#[test]
fn build_initramfs_hello_padded_to_lands_at_target_size() {
    for target in [596usize, 600, 604, 612] {
        let cpio = build_initramfs_hello_padded_to(target);
        let filesize_field = std::str::from_utf8(&cpio[6 + 6 * 8..6 + 7 * 8]).unwrap();
        let init_size = usize::from_str_radix(filesize_field, 16).unwrap();
        assert_eq!(
            init_size, target,
            "padded-hello target={target}: cpio reports /init size {init_size}"
        );
    }
}

#[test]
#[ignore = "/init=600 minimal reproducer (hangs per `4c68139`); ~9 min"]
fn linux_userspace_hello_padded_to_600_milestone() {
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
    let cpio = build_initramfs_hello_padded_to_600();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(
        &mut vm,
        b"HELLO FROM USERSPACE",
        16_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "HELLO not seen — minimal /init=600 reproduces the stall (good — \
             use this as the canonical minimal reproducer for future debug); {}",
            dump_uart_on_failure(&cumulative, "hello-600")
        )
    });
    eprintln!(
        "HELLO seen at {steps} steps — /init=600 boots fast on this run; \
         the size-trigger has been fixed or our emulator state changed \
         (was confirmed-hanging in `4c68139`'s test run)"
    );
}

/// Test the "mod 8" hypothesis. Observed: {600 mod 8 = 0, 602
/// mod 8 = 2} hang; {601, 604, 605} all have mod 8 ∉ {0, 2} and
/// pass. /init at size 608 (= 8 × 76, mod 8 = 0) tests whether
/// the bad set is "N mod 8 ∈ {0, 2}" or just the singletons
/// {600, 602}.
#[test]
#[ignore = "mod-8 counter-example (size 608 passes per `a915ec3`); ~52 s"]
fn linux_userspace_hello_padded_to_608_milestone() {
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
    let cpio = build_initramfs_hello_padded_to(608);
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(
        &mut vm,
        b"HELLO FROM USERSPACE",
        16_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "/init=608 HANGS — mod-8 hypothesis holds! Bad set is much wider \
             than {{600, 602}}: every size with N mod 8 ∈ {{0, 2}}; {}",
            dump_uart_on_failure(&cumulative, "hello-608")
        )
    });
    eprintln!(
        "/init=608 passes at {steps} steps — mod-8 hypothesis WRONG; \
         bad set really is just {{600, 602}} (or similar tiny set)"
    );
}

/// Companion to `build_initramfs_uname_lands_at_a_known_bad_binary_size`:
/// pin the known-good points where `build_initramfs_hello_padded_to`
/// lands for the boundary sizes our bisection probes used. If
/// the helper's arithmetic regresses, the next debug pass would
/// land at the wrong size and the integration result wouldn't
/// match the recorded bisection.
#[test]
fn build_initramfs_hello_padded_to_known_good_sizes() {
    // (target, expected /init size from cpio header — same as target)
    for target in [601usize, 604, 605, 608] {
        let cpio = build_initramfs_hello_padded_to(target);
        let filesize_field = std::str::from_utf8(&cpio[6 + 6 * 8..6 + 7 * 8]).unwrap();
        let init_size = usize::from_str_radix(filesize_field, 16).unwrap();
        assert_eq!(
            init_size, target,
            "padded-hello target={target} landed at {init_size}; \
             would break the bisection's recorded outcome"
        );
        // And the known-good ones MUST NOT be in the bad set.
        assert!(
            !matches!(init_size, 600 | 602),
            "padded-hello target={target} landed at known-bad {init_size}"
        );
    }
}

/// Probe whether /init binary size 601 (the only untested point
/// in the bad range — 600 and 602 are confirmed-hanging) is also
/// bad. If this hangs, the entire 600..=602 range is direct
/// evidence; if it passes, the bug is sparse and 601 is a "gap"
/// in the bad range — which would be a wild new finding.
#[test]
#[ignore = "601-byte boundary probe (passes per `01bc97a`); ~52 s"]
fn linux_userspace_hello_padded_to_601_milestone() {
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
    let cpio = build_initramfs_hello_padded_to(601);
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(
        &mut vm,
        b"HELLO FROM USERSPACE",
        16_000_000_000,
        &mut cumulative,
    )
    .unwrap_or_else(|()| {
        panic!(
            "HELLO not seen at /init=601 — entire 600..=602 range is now direct evidence; {}",
            dump_uart_on_failure(&cumulative, "hello-601")
        )
    });
    eprintln!(
        "HELLO seen at {steps} steps — /init=601 PASSES (bad range is sparse: 600 and 602 hang, \
         601 works — wild new finding, bisection needs follow-up)"
    );
}

/// Pin `build_initramfs_uname` as the canonical reproducer of the
/// /init-binary-size-600 stall: extract the /init filesize from
/// the cpio header (field 6 of the 13 ASCII-hex fields after the
/// 6-byte magic) and assert it equals one of the known-bad
/// sizes (600 or 602; 601 works, see `d6fd05f`). If a future
/// refactor accidentally changes the code layout enough to push
/// the binary out of the bad set, the "canonical reproducer"
/// loses its bug-reproducing property and this test fails —
/// prompting a re-bisection.
#[test]
fn build_initramfs_uname_lands_at_a_known_bad_binary_size() {
    let cpio = build_initramfs_uname();
    let filesize_field = std::str::from_utf8(&cpio[6 + 6 * 8..6 + 7 * 8]).unwrap();
    let init_size = u32::from_str_radix(filesize_field, 16).unwrap();
    assert!(
        matches!(init_size, 600 | 602),
        "build_initramfs_uname produced /init of size {init_size}; \
         expected 600 or 602 (the confirmed-hanging sizes; 601 works \
         per `d6fd05f`)"
    );
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

/// Build a cpio whose /init calls `sys_time(NULL)` (syscall 13,
/// i.e. `sys_time32` on i386), stores the returned 32-bit time_t
/// in a buffer, and writes the buffer to stdout bracketed by a
/// pair of unique markers. Exercises a syscall the existing
/// milestones don't (hello + proc/version cover write, exit,
/// mount, open, read — but no clock primitive) and proves the
/// kernel's wall-clock subsystem (CMOS RTC read at early boot
/// then jiffies updates) surfaces through the int 0x80 trampoline
/// in a form userspace can decode. The trailing
/// `[USERSPACE END]` marker exists so the test can wait until
/// the entire 4-byte time_t has been flushed to UART before
/// trying to decode it (without it the marker_pre and partial
/// time bytes could be in cumulative while the rest sits in
/// the in-flight chunk).
fn build_initramfs_time() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE TIME=";
    let marker_post: &[u8] = b"]\n[USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // write(1, marker_pre, marker_pre.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_time(NULL) — sys 13. Returns time_t in eax.
        out.extend_from_slice(&[0xB8, 0x0D, 0x00, 0x00, 0x00]); // mov eax, 13
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (tloc = NULL)
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf_addr], eax — A3 = MOV moffs32, EAX. Stores
        // the 32-bit time_t in the writable data segment so the
        // next write(2) can pick it up; needs RWX p_flags.
        out.push(0xA3);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        // write(1, buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, marker_post.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr);
    let buf_zeros = [0u8; 4];
    let binary = make_init_elf32(
        &code,
        &[marker_pre, marker_post, &buf_zeros],
        7, // R | W | X — /init writes time_t into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_getpid` (syscall 20), stores
/// the returned 32-bit PID in a buffer, and writes the buffer to
/// stdout bracketed by markers. Same shape as `build_initramfs_time`
/// (no-arg syscall returning a u32 in eax → store via `mov ds:[],
/// eax` → write 4 bytes), so the code structure is intentionally
/// parallel. What's different is what's pinned: the kernel must
/// have a task struct for PID 1, and `sys_getpid` returns its `pid`
/// field via the syscall ABI. /init is always PID 1 — a value
/// stable enough that the test asserts exactly `== 1`, unlike the
/// time milestone's range check.
fn build_initramfs_getpid() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE PID=";
    let marker_post: &[u8] = b"]\n[USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // write(1, marker_pre, marker_pre.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_getpid() — sys 20. Returns pid in eax. No args.
        out.extend_from_slice(&[0xB8, 0x14, 0x00, 0x00, 0x00]); // mov eax, 20
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf_addr], eax — A3 = MOV moffs32, EAX.
        out.push(0xA3);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        // write(1, buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, marker_post.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr);
    let buf_zeros = [0u8; 4];
    let binary = make_init_elf32(
        &code,
        &[marker_pre, marker_post, &buf_zeros],
        7, // R | W | X — /init writes pid into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_gettimeofday(&tv, NULL)`
/// (syscall 78) and writes the kernel-filled 8-byte struct to
/// stdout bracketed by markers. Unlike the time milestone (which
/// only proves the kernel returns time_t in eax), this one
/// exercises the *struct-to-userspace* path: the kernel must
/// `copy_to_user` a `struct timeval { tv_sec; tv_usec }` into
/// /init's writable data segment. Same write target as the
/// proc/version reader, but for a kernel-synthesized struct.
/// The microseconds field also pins sub-second resolution —
/// `sys_time` rounds away anything finer than 1 s.
fn build_initramfs_gettimeofday() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE TV=";
    let marker_post: &[u8] = b"]\n[USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // write(1, marker_pre, marker_pre.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_gettimeofday(tv=buf, tz=NULL) — sys 78. Kernel
        // copy_to_user's `struct timeval` (8 bytes: sec, usec)
        // into buf. eax=0 on success.
        out.extend_from_slice(&[0xB8, 0x4E, 0x00, 0x00, 0x00]); // mov eax, 78
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes()); // ebx = &tv
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (tz = NULL)
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 8)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // edx = 8
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, marker_post.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr);
    let buf_zeros = [0u8; 8]; // struct timeval = 8 bytes
                              // Tail-pad dodges a NEW bad-set hit found 2026-05-29: this
                              // builder, before the pad, produced an /init of exactly 213
                              // bytes — and that size triggers the same kernel-stalls-in-
                              // pata_legacy-probe symptom as the {600, 602} bisection
                              // (full kernel boot, "Trying to unpack rootfs image as
                              // initramfs..." appears, then nothing — no "Run /init",
                              // no userspace, budget exhausts at 8.7 min). The original
                              // bisection only swept sizes 588..608, so it missed that
                              // the bad set extends much further. With this 12-byte
                              // tail pad the binary is 225 bytes and the test passes
                              // in ~52 s. The blocker note in README has the wider
                              // diagnosis. The bytes are unreferenced by /init code.
    let tail_pad = [0u8; 12];
    let binary = make_init_elf32(
        &code,
        &[marker_pre, marker_post, &buf_zeros, &tail_pad],
        7, // R | W | X — kernel copy_to_user writes timeval into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
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

/// Clock-primitive milestone: /init calls `sys_time(NULL)`,
/// stores the returned time_t in a writable buffer, and writes
/// `[USERSPACE TIME=<4-byte-le-t>]\n[USERSPACE END]\n` to UART.
/// Proves the kernel's wall-clock subsystem (CMOS RTC pulled at
/// boot + jiffies updates) reaches userspace through int 0x80.
/// Two-stage check: first wait for the `[USERSPACE END]` marker
/// (which guarantees marker_pre + 4-byte t + marker_post are all
/// in `cumulative`), then locate marker_pre and decode the
/// trailing 4 bytes as little-endian u32. Asserts the value is
/// neither 0 (kernel returned a zero clock — RTC not read) nor
/// -1 (syscall returned an errno — handler broken).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_time_milestone() {
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
    let cpio = build_initramfs_time();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE TIME=";
    // /init writes `]\n[USERSPACE END]\n` but the kernel's TTY
    // line discipline ONLCR-translates every `\n` to `\r\n`
    // before it hits UART. The bytes the test sees in
    // `cumulative` are the kernel-emitted form, so the search
    // needle includes the `\r`. (Empirically confirmed from
    // /tmp/wwwvm-userspace-time-failure.bin: `5d 0d 0a 5b ...`).
    // marker_pre stays `\n`-free so its match is unaffected.
    let marker_post_search: &[u8] = b"]\r\n[USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "time")
            )
        });
    eprintln!("sys_time userspace milestone seen after {steps} steps");

    // Locate marker_pre and the 4-byte time_t that follows. We
    // know marker_post landed in `cumulative`, so marker_pre and
    // its 4-byte payload must be earlier in the same buffer.
    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let t_off = pre_pos + marker_pre.len();
    let t_bytes: [u8; 4] = cumulative[t_off..t_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let time_t = u32::from_le_bytes(t_bytes);
    eprintln!("sys_time returned time_t = {time_t} (0x{time_t:08X})");
    // The kernel's rtc_cmos initializes the system clock to
    // 2020-01-01T00:00:00 UTC = 1577836800 (printed in the
    // boot log as `rtc_cmos rtc_cmos: setting system clock to
    // 2020-01-01T00:00:00 UTC (1577836800)`). By the time /init
    // runs the clock has advanced a few seconds via jiffies, so
    // observed time_t is consistently `1577836800 + k` for small
    // k (e.g. 9 on the first green run). Assert `>= 2020-01-01`
    // and `< 2038-01-19` (i386 sys_time32 hard wall) so the
    // test catches both an under-initialized clock (0, partial
    // RTC read) and a sign-extended errno that happens to land
    // in a plausible-looking range.
    const RTC_CMOS_FLOOR: u32 = 1_577_836_800; // 2020-01-01 UTC
    const Y2038_CEIL: u32 = 0x7FFF_FFFF; // i386 sys_time32 wraps here
    assert!(
        (RTC_CMOS_FLOOR..Y2038_CEIL).contains(&time_t),
        "sys_time returned {time_t} (0x{time_t:08X}); expected \
         [{RTC_CMOS_FLOOR}, {Y2038_CEIL}); {}",
        dump_uart_on_failure(&cumulative, "time-range")
    );
}

/// Task-struct primitive milestone: /init calls `sys_getpid`
/// (syscall 20), writes the returned 32-bit pid bracketed by
/// `[USERSPACE PID=]` markers. The shape mirrors the time
/// milestone (no-arg syscall → eax → 4-byte buffer → write),
/// but what's pinned is the kernel's task-struct path, not its
/// clock subsystem. /init is always PID 1 — the test asserts
/// exactly `== 1` rather than a range, because any deviation
/// here would mean the kernel is exec'ing /init under a
/// different task struct (or the syscall ABI mis-routes its
/// return).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_getpid_milestone() {
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
    let cpio = build_initramfs_getpid();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE PID=";
    // ONLCR: kernel TTY translates /init's `\n` into `\r\n` before
    // it hits UART. Search needle uses the kernel-emitted form.
    let marker_post_search: &[u8] = b"]\r\n[USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "getpid")
            )
        });
    eprintln!("sys_getpid userspace milestone seen after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let p_off = pre_pos + marker_pre.len();
    let p_bytes: [u8; 4] = cumulative[p_off..p_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let pid = u32::from_le_bytes(p_bytes);
    eprintln!("sys_getpid returned pid = {pid}");
    assert_eq!(
        pid,
        1,
        "sys_getpid returned {pid} (expected 1 — /init is PID 1); {}",
        dump_uart_on_failure(&cumulative, "getpid-wrong")
    );
}

/// Sub-second-clock milestone: /init calls
/// `sys_gettimeofday(&tv, NULL)` (syscall 78), and the kernel
/// fills an 8-byte `struct timeval` in /init's data segment.
/// /init writes the raw 8 bytes between `[USERSPACE TV=]` and
/// `[USERSPACE END]` markers; the test decodes
/// `(tv_sec, tv_usec) = (u32_le, u32_le)` and asserts both
/// fields are in plausible ranges (sec in the same window as
/// the time milestone, usec strictly < 1_000_000).
///
/// What's new vs `linux_userspace_time_milestone`:
///   - struct-to-userspace path (kernel `copy_to_user`s
///     into the buf, vs returning a u32 in eax)
///   - sub-second resolution — `sys_time` rounds away
///     anything finer than 1 s; gettimeofday returns
///     microseconds, so a non-zero `usec` proves the
///     kernel's internal clock is actually finer-grained
///     than 1 Hz, not just snapped to it.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_gettimeofday_milestone() {
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
    let cpio = build_initramfs_gettimeofday();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE TV=";
    let marker_post_search: &[u8] = b"]\r\n[USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "gettimeofday")
            )
        });
    eprintln!("sys_gettimeofday userspace milestone seen after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let t_off = pre_pos + marker_pre.len();
    let sec_bytes: [u8; 4] = cumulative[t_off..t_off + 4]
        .try_into()
        .expect("4 bytes for tv_sec");
    let usec_bytes: [u8; 4] = cumulative[t_off + 4..t_off + 8]
        .try_into()
        .expect("4 bytes for tv_usec");
    let tv_sec = u32::from_le_bytes(sec_bytes);
    let tv_usec = u32::from_le_bytes(usec_bytes);
    eprintln!("sys_gettimeofday returned tv_sec = {tv_sec}, tv_usec = {tv_usec}");

    // Same bounds as the time milestone: kernel sets clock to
    // 2020-01-01 at boot, Y2038 is the i386 wall.
    const RTC_CMOS_FLOOR: u32 = 1_577_836_800;
    const Y2038_CEIL: u32 = 0x7FFF_FFFF;
    assert!(
        (RTC_CMOS_FLOOR..Y2038_CEIL).contains(&tv_sec),
        "tv_sec = {tv_sec} (0x{tv_sec:08X}); expected [{RTC_CMOS_FLOOR}, {Y2038_CEIL}); {}",
        dump_uart_on_failure(&cumulative, "gettimeofday-sec")
    );
    // tv_usec ∈ [0, 1_000_000) by `gettimeofday(2)` contract.
    // Anything outside this range means either the kernel left
    // the field uninitialized (e.g. wrote only tv_sec then
    // failed) or the copy_to_user wrote into the wrong slot.
    assert!(
        tv_usec < 1_000_000,
        "tv_usec = {tv_usec} (0x{tv_usec:08X}); expected < 1_000_000; {}",
        dump_uart_on_failure(&cumulative, "gettimeofday-usec")
    );
}
