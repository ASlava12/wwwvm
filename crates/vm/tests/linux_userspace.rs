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
//!   - `linux_userspace_fork_milestone` — process creation:
//!     /init calls `sys_fork` (syscall 2), then `sys_waitpid`
//!     to order parent-after-child, and BOTH processes write
//!     `[USERSPACE FORK ret=<eax>][USERSPACE END]`. Test
//!     verifies both occurrences in UART and asserts the values
//!     are (0, child_PID_in_parent_view). Pins kernel
//!     copy_process + scheduler + return-from-fork in child.
//!
//!   - `linux_userspace_execve_milestone` — second-binary load:
//!     /init forks, child calls `sys_execve("/helper", NULL, NULL)`,
//!     /helper writes `[USERSPACE EXECVE_OK]` and exits. Initramfs
//!     gets TWO files (init + helper) via
//!     `build_cpio_archive_with_helper`. Pins path-resolving
//!     execve: VFS lookup in initramfs, ELF parse + new mm setup,
//!     jump to /helper's entry. If execve had returned to child
//!     (failure), the FAILED marker would have shown instead.
//!
//!   - `linux_userspace_execve_chain_milestone` — execve from
//!     an already-exec'd image: /init forks; child execve's /h1;
//!     /h1 execve's /h2; /h2 writes `[USERSPACE H2_OK]` and exits.
//!     Two distinct FAILED markers (one per hop) identify which
//!     hop broke if the test fails. Pins execve being callable
//!     from a process started via execve itself (not a one-shot
//!     post-fork-only fast path), and a second mm-swap that
//!     follows the first.
//!
//!   - `linux_userspace_brk_milestone` — process memory layout:
//!     /init calls `sys_brk(0)` (syscall 45, ebx=0 = query),
//!     writes the returned program break between
//!     `[USERSPACE BRK=]` markers. Test asserts the value lands
//!     in `[INIT_LOAD_ADDR, 0xC0000000)` (above /init's load
//!     address, below the kernel split) and is page-aligned.
//!     Pins `mm->brk` initialization — the kernel sets it to
//!     the end of the data segment, rounded up to a page, when
//!     it sets up the new process's mm.
//!
//!   - `linux_userspace_brk_extend_milestone` — on-demand heap
//!     allocation: /init queries the current break, requests
//!     `+0x1000` (one page), and the test asserts the new break
//!     equals `old + 0x1000` exactly. brk(2) returns the new
//!     break on success OR the current (unchanged) break on
//!     failure, so a kernel that rejected the grow shows as
//!     `new == old` and the assert fires.
//!
//!   - `linux_userspace_argv_milestone` — process-startup ABI:
//!     /init forks; child execve's /helper with argv
//!     `["/helper", "ARG1"]`; /helper reads `argv[1]` off
//!     `[esp+8]` and writes the string between markers. Test
//!     asserts the 4 bytes equal `b"ARG1"`. Pins the kernel's
//!     `copy_strings` path in execve (skipped when argv is
//!     NULL) plus the i386 SysV stack layout at entry.
//!
//!   - `linux_userspace_envp_milestone` — environment variables:
//!     /init execve's /helper with argv `["/helper"]` and envp
//!     `["KEY=VAL"]`; /helper reads `envp[0]` off `[esp+0x0C]`
//!     (after argc + 1-arg argv + NULL) and writes the 7 bytes
//!     "KEY=VAL" bracketed. Same `copy_strings` mechanism as
//!     argv but for the envp half — argv test alone doesn't
//!     pin it.
//!
//!   - `linux_userspace_mmap_milestone` — anonymous mmap path:
//!     /init asks the kernel for one anonymous page via
//!     `sys_mmap2`, writes a sentinel byte (0x42), reads it
//!     back, and writes `[USERSPACE MMAP=<addr>VAL=<byte>][USERSPACE END]`.
//!     Test asserts addr lands in userspace + page-aligned and
//!     the byte round-trips. Different mechanism than brk:
//!     mmap allocates a fresh VMA at a kernel-chosen address;
//!     brk extends the heap region contiguous with the data
//!     segment. glibc's malloc uses both.
//!
//!   - `linux_userspace_file_io_milestone` — file-create round-
//!     trip: /init opens "/test_file" with O_CREAT|O_WRONLY,
//!     writes "TESTDATA", closes, reopens read-only, reads it
//!     back, and writes `[USERSPACE FILE=<8 bytes>][USERSPACE END]`.
//!     Pins writable initramfs (tmpfs), inode creation through
//!     `do_filp_open` with O_CREAT, `sys_close` (never used
//!     before), and cross-fd persistence via the page cache.
//!
//!   - `linux_userspace_stat_milestone` — file metadata: /init
//!     creates `/probe` with 8 bytes, calls `sys_stat64`, reads
//!     `st_size` (offset 44 in struct stat64 on i386), writes
//!     it between `[USERSPACE STAT_SIZE=…][USERSPACE END]`.
//!     Pins inode metadata via `cp_new_stat64` + `copy_to_user`,
//!     and the kernel updating `i_size` after the write.
//!
//!   - `linux_userspace_lseek_milestone` — random-access I/O:
//!     /init creates `/probe` with 8 bytes "TESTDATA", calls
//!     `sys_lseek(fd, 4, SEEK_SET)`, reads 4 bytes — should be
//!     `b"DATA"` (offsets 4..8). Test asserts the bytes equal
//!     "DATA". Pins the kernel's `struct file` position
//!     bookkeeping; sequential read at offset 0 doesn't.
//!
//!   - `linux_userspace_dup2_milestone` — fd duplication for
//!     shell-style redirection: /init opens `/log`, dup2's it
//!     onto fd 1, writes "REDIRECTED" via fd 1 (should land in
//!     /log, NOT UART), closes, reopens /log, reads back, prints
//!     via fd 2 (stderr → /dev/console). Test asserts the 10
//!     bytes round-trip — if dup2 didn't take effect, /log would
//!     be empty and the assert fires. Foundation for `cmd > file`.
//!
//!   - `linux_userspace_unlink_milestone` — file deletion +
//!     `-errno` return path: /init creates `/probe`, calls
//!     `sys_unlink`, then `sys_stat64("/probe", …)` which MUST
//!     fail with `-ENOENT`. Test asserts the stat return is
//!     `0xFFFFFFFE` (= -2 sign-extended). First milestone to
//!     verify a syscall FAILURE — proves the negative-result
//!     ABI works the same as success returns.
//!
//!   - `linux_userspace_mkdir_milestone` — directory creation +
//!     multi-level path resolution: /init creates `/dir` via
//!     `sys_mkdir`, then `/dir/test` (file-in-subdirectory),
//!     writes "INDIR", reads back, prints. Test asserts the
//!     5 bytes round-trip equal `b"INDIR"`. Every prior file
//!     test used flat paths at `/`; this one walks two levels.
//!
//!   - `linux_userspace_nanosleep_milestone` — timer wakeup:
//!     /init samples `sys_time` before and after `sys_nanosleep(1
//!     sec)`, prints both. Test asserts `t1 > t0` — proves the
//!     kernel held the process off for ≥ 1 second of guest wall
//!     time. Pins PIT/LAPIC IRQs → jiffies → timer wheel →
//!     task wakeup end-to-end. Until now every milestone ran
//!     in "instant" guest time. NOTE: the test strips `\r\n`
//!     ONLCR translation when extracting binary u32 from UART
//!     — a `0x0A` byte in t0/t1 was being padded by the kernel
//!     TTY's `0x0D` and shifting the decoded value. Reusable
//!     pattern for any future binary-data milestone.
//!
//!   - `linux_userspace_writev_milestone` — vectored I/O: /init
//!     calls `sys_writev` (syscall 146) with a 2-element
//!     `iovec[]` whose entries together spell
//!     `[USERSPACE WRITEV=AB][USERSPACE END]\n`. Test asserts
//!     the concatenated string appears in UART. Pins the
//!     kernel's iovec walker — every prior write milestone used
//!     a single contiguous buffer.
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

/// /init binary sizes that trigger the
/// "stalls-in-pata_legacy-probe, kernel never reaches /init exec"
/// boot bug. The 17-probe bisection around 600 found {600, 602};
/// adding `gettimeofday_milestone` on 2026-05-29 surfaced 213
/// (same symptoms, very different size — so the bad set is wider
/// than the bisection's 588..608 window suggested). Treat this
/// list as a non-exhaustive lower bound; landing at a previously
/// untested size means another 8-minute discovery if it's also
/// bad. The blocker note in README has the full diagnosis.
const KNOWN_BAD_INIT_SIZES: &[usize] = &[213, 600, 602];

/// Wrap `make_init_elf32` with a known-bad-size dodge for
/// production milestones. If the raw ELF lands at a size in
/// `KNOWN_BAD_INIT_SIZES`, append 4 zero bytes as an extra data
/// segment so PHDR's `p_filesz` and the cpio's `c_filesize`
/// grow together (the bug seems to depend on `c_filesize` in
/// the unpacker, but keeping both in sync hedges against the
/// alternative). Bisection probes and `build_initramfs_uname`
/// (which intentionally hangs at 600) call `make_init_elf32`
/// directly to preserve their specific sizes; everything else
/// should funnel through this.
fn make_init_elf32_safe(code: &[u8], data_segments: &[&[u8]], pt_flags: u32) -> Vec<u8> {
    let binary = make_init_elf32(code, data_segments, pt_flags);
    if !KNOWN_BAD_INIT_SIZES.contains(&binary.len()) {
        return binary;
    }
    // Rebuild with a 4-byte tail-pad. 4 bytes is enough to step
    // past adjacent bad sizes ({600, 602} are 2 apart but 213
    // is isolated in our current sample); if both N and N+4 are
    // bad we'll discover it on the next surprise — the function
    // asserts the dodge succeeded in debug builds so a future
    // discovery panics loudly rather than hanging silently.
    let pad = [0u8; 4];
    let mut segs: Vec<&[u8]> = data_segments.to_vec();
    segs.push(&pad);
    let binary = make_init_elf32(code, &segs, pt_flags);
    debug_assert!(
        !KNOWN_BAD_INIT_SIZES.contains(&binary.len()),
        "make_init_elf32_safe: +4-byte pad still landed at known-bad size {}; \
         widen KNOWN_BAD_INIT_SIZES or change the pad amount",
        binary.len()
    );
    binary
}

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

/// Same as `build_cpio_archive` but adds a second executable
/// file at the root with the given name and bytes. Used by the
/// execve milestone: /init forks, child calls `sys_execve` on
/// the second binary, the second binary writes a marker. The
/// second file gets the same 0o100_755 mode as /init so the
/// kernel's `do_execve` path treats it identically.
fn build_cpio_archive_with_helper(
    init_binary: &[u8],
    helper_name: &str,
    helper_binary: &[u8],
) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry(helper_name, helper_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio_entry("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    archive
}

/// Same shape as `build_cpio_archive_with_helper` but with THREE
/// executables — /init plus two more named binaries. Used by the
/// execve-chain milestone, where /init forks + child execves /h1,
/// /h1 in turn execves /h2, and /h2 is the one that writes the OK
/// marker. Pins the harder version of execve: it can be called
/// from a non-PID-1 process that was *itself* started via
/// execve (not via initramfs boot exec).
fn build_cpio_archive_with_two_helpers(
    init_binary: &[u8],
    h1_name: &str,
    h1_binary: &[u8],
    h2_name: &str,
    h2_binary: &[u8],
) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry(h1_name, h1_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry(h2_name, h2_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
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
    let binary = make_init_elf32_safe(&code, &[msg], 5);
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
    let fork = build_initramfs_fork();
    let execve = build_initramfs_execve();
    let execve_chain = build_initramfs_execve_chain();
    let brk = build_initramfs_brk();
    let brk_extend = build_initramfs_brk_extend();
    let argv = build_initramfs_argv();
    let envp = build_initramfs_envp();
    let mmap = build_initramfs_mmap();
    let file_io = build_initramfs_file_io();
    let stat = build_initramfs_stat();
    let lseek = build_initramfs_lseek();
    let dup2 = build_initramfs_dup2();
    let unlink = build_initramfs_unlink();
    let mkdir = build_initramfs_mkdir();
    let chdir = build_initramfs_chdir();
    let nanosleep = build_initramfs_nanosleep();
    let writev = build_initramfs_writev();
    assert_eq!(&uname[0..6], b"070701");
    assert_eq!(&proc_version[0..6], b"070701");
    assert_eq!(&hello[0..6], b"070701");
    assert_eq!(&time[0..6], b"070701");
    assert_eq!(&getpid[0..6], b"070701");
    assert_eq!(&gettimeofday[0..6], b"070701");
    assert_eq!(&fork[0..6], b"070701");
    assert_eq!(&execve[0..6], b"070701");
    assert_eq!(&execve_chain[0..6], b"070701");
    assert_eq!(&brk[0..6], b"070701");
    assert_eq!(&brk_extend[0..6], b"070701");
    assert_eq!(&argv[0..6], b"070701");
    assert_eq!(&envp[0..6], b"070701");
    assert_eq!(&mmap[0..6], b"070701");
    assert_eq!(&file_io[0..6], b"070701");
    assert_eq!(&stat[0..6], b"070701");
    assert_eq!(&lseek[0..6], b"070701");
    assert_eq!(&dup2[0..6], b"070701");
    assert_eq!(&unlink[0..6], b"070701");
    assert_eq!(&mkdir[0..6], b"070701");
    assert_eq!(&chdir[0..6], b"070701");
    assert_eq!(&nanosleep[0..6], b"070701");
    assert_eq!(&writev[0..6], b"070701");
    if std::env::var_os("WWWVM_DUMP_INIT_ARTIFACTS").is_some() {
        let _ = std::fs::write("/tmp/wwwvm-uname.cpio", &uname);
        let _ = std::fs::write("/tmp/wwwvm-proc-version.cpio", &proc_version);
        let _ = std::fs::write("/tmp/wwwvm-hello.cpio", &hello);
        let _ = std::fs::write("/tmp/wwwvm-time.cpio", &time);
        let _ = std::fs::write("/tmp/wwwvm-getpid.cpio", &getpid);
        let _ = std::fs::write("/tmp/wwwvm-gettimeofday.cpio", &gettimeofday);
        let _ = std::fs::write("/tmp/wwwvm-fork.cpio", &fork);
        let _ = std::fs::write("/tmp/wwwvm-execve.cpio", &execve);
        let _ = std::fs::write("/tmp/wwwvm-execve-chain.cpio", &execve_chain);
        let _ = std::fs::write("/tmp/wwwvm-brk.cpio", &brk);
        let _ = std::fs::write("/tmp/wwwvm-brk-extend.cpio", &brk_extend);
        let _ = std::fs::write("/tmp/wwwvm-argv.cpio", &argv);
        let _ = std::fs::write("/tmp/wwwvm-envp.cpio", &envp);
        let _ = std::fs::write("/tmp/wwwvm-mmap.cpio", &mmap);
        let _ = std::fs::write("/tmp/wwwvm-file-io.cpio", &file_io);
        let _ = std::fs::write("/tmp/wwwvm-stat.cpio", &stat);
        let _ = std::fs::write("/tmp/wwwvm-lseek.cpio", &lseek);
        let _ = std::fs::write("/tmp/wwwvm-dup2.cpio", &dup2);
        let _ = std::fs::write("/tmp/wwwvm-unlink.cpio", &unlink);
        let _ = std::fs::write("/tmp/wwwvm-mkdir.cpio", &mkdir);
        let _ = std::fs::write("/tmp/wwwvm-chdir.cpio", &chdir);
        let _ = std::fs::write("/tmp/wwwvm-nanosleep.cpio", &nanosleep);
        let _ = std::fs::write("/tmp/wwwvm-writev.cpio", &writev);
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

/// Pins the safe-wrapper's invariant: any input that would
/// land at a `KNOWN_BAD_INIT_SIZES` entry gets shifted off by
/// `make_init_elf32_safe`, and any input that wouldn't is
/// returned byte-identical. Cheap fast unit test — no
/// vmlinuz required, runs in default `cargo test`. Future
/// production builders that funnel through `_safe` rely on
/// this dodge; if the wrapper regresses, every production
/// milestone could silently start hanging again.
#[test]
fn make_init_elf32_safe_dodges_known_bad_sizes() {
    // Construct a code segment that lands at each known-bad size.
    // The bare ELF+PHDR is 84 bytes, so a `code` of `N - 84` bytes
    // (with empty `data_segments`) produces a binary of exactly N.
    for &bad in KNOWN_BAD_INIT_SIZES {
        let code = vec![0x90u8; bad - 84];
        let raw = make_init_elf32(&code, &[], 5);
        assert_eq!(
            raw.len(),
            bad,
            "raw make_init_elf32 should land at exactly {bad} for this construction",
        );
        let safe = make_init_elf32_safe(&code, &[], 5);
        assert_ne!(
            safe.len(),
            bad,
            "make_init_elf32_safe should dodge bad size {bad}",
        );
        assert!(
            !KNOWN_BAD_INIT_SIZES.contains(&safe.len()),
            "make_init_elf32_safe dodge landed at another bad size {} \
             (came from {bad}); widen the pad or the BAD list",
            safe.len(),
        );
    }
    // A confirmed-good size (217 — the gettimeofday production
    // milestone's actual binary size after its own safe dodge)
    // must be returned untouched.
    let code = vec![0x90u8; 217 - 84];
    let raw = make_init_elf32(&code, &[], 5);
    assert_eq!(raw.len(), 217);
    let safe = make_init_elf32_safe(&code, &[], 5);
    assert_eq!(
        safe.len(),
        217,
        "good size 217 should pass through untouched, got {}",
        safe.len(),
    );
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
    let binary = make_init_elf32_safe(
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
    let binary = make_init_elf32_safe(
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
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_post, &buf_zeros],
        7, // R | W | X — /init writes pid into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_brk(0)` (syscall 45,
/// ebx=0 = query) and writes the returned program break
/// bracketed by `[USERSPACE BRK=]` markers. Same shape as the
/// time/getpid milestones (no-arg-ish syscall returning a u32 →
/// MOV moffs32, EAX → write 4 bytes) but pins a different kernel
/// subsystem: process memory layout. The returned break must be
/// at or above /init's binary end (LOAD_ADDR + binary length,
/// rounded up to a page) — anything else means the kernel
/// didn't set up `mm->brk` for the process.
fn build_initramfs_brk() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE BRK=";
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
        // sys_brk(0) — sys 45. ebx=0 means "query current break".
        // Returns the current break address in eax (32-bit on i386).
        out.extend_from_slice(&[0xB8, 0x2D, 0x00, 0x00, 0x00]); // mov eax, 45
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf_addr], eax
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
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_post, &buf_zeros],
        7, // R | W | X — /init writes brk into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init queries the current break with
/// `sys_brk(0)`, then asks the kernel to GROW the heap by one
/// page with `sys_brk(old_break + 0x1000)`, and writes both
/// returns between markers:
///
///   `[USERSPACE BRK_OLD=<4 bytes>BRK_NEW=<4 bytes>END]`
///
/// Test asserts `new == old + 0x1000` — proves the kernel
/// actually allocates a new page on demand, not just returns the
/// queried value. If the heap can't grow, kernel returns the
/// CURRENT brk (per the brk(2) contract: returns new break on
/// success OR current break on failure), and the test catches
/// `new == old` which would mean growth was rejected.
fn build_initramfs_brk_extend() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE BRK_OLD=";
    let marker_mid: &[u8] = b"BRK_NEW=";
    let marker_end: &[u8] = b"END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      buf_old_addr: u32,
                      buf_new_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_brk(0) — query current break. Returns eax = brk.
        out.extend_from_slice(&[0xB8, 0x2D, 0x00, 0x00, 0x00]); // mov eax, 45
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf_old], eax — stash original
        out.push(0xA3);
        out.extend_from_slice(&buf_old_addr.to_le_bytes());
        // ebx = eax + 0x1000 — request +1 page
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0x81, 0xC3, 0x00, 0x10, 0x00, 0x00]); // add ebx, 0x1000
                                                                      // sys_brk(new_brk) — set break.
        out.extend_from_slice(&[0xB8, 0x2D, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf_new], eax
        out.push(0xA3);
        out.extend_from_slice(&buf_new_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf_old, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_old_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_mid, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_mid_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_mid.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf_new, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_new_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_end, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_end_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_end.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_end_addr = marker_mid_addr + marker_mid.len() as u32;
    let buf_old_addr = marker_end_addr + marker_end.len() as u32;
    let buf_new_addr = buf_old_addr + 4;
    let code = build_code(
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        buf_old_addr,
        buf_new_addr,
    );
    let buf_old = [0u8; 4];
    let buf_new = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_mid, marker_end, &buf_old, &buf_new],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_mmap2(NULL, 0x1000,
/// PROT_READ|PROT_WRITE, MAP_ANONYMOUS|MAP_PRIVATE, -1, 0)` —
/// syscall 192, asks the kernel for one anonymous page —
/// stores the returned address, writes a sentinel byte (0x42)
/// to it, reads the same byte back, then prints both bracketed
/// by markers:
///
///   `[USERSPACE MMAP=<4 bytes addr>VAL=<1 byte>][USERSPACE END]`
///
/// Two assertions in the test:
///   - addr lands in `[INIT_LOAD_ADDR, 0xC0000000)` and is
///     page-aligned (low 12 bits zero) — proves mmap actually
///     allocated a userspace page rather than returning -errno
///   - the byte read back equals 0x42 — proves the page is
///     genuinely backed by RAM (not a CoW-zero stub), and that
///     userspace can both write and read it
///
/// Different mechanism than brk: brk extends the heap region
/// contiguous with the data segment; mmap allocates a fresh
/// VMA at an arbitrary virtual address chosen by the kernel.
/// glibc's malloc uses both, depending on allocation size.
fn build_initramfs_mmap() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE MMAP=";
    let marker_mid: &[u8] = b"VAL=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      buf_addr: u32,
                      buf_val_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_mmap2(addr=NULL, len=0x1000, prot=R|W=3,
        //           flags=MAP_ANONYMOUS|MAP_PRIVATE=0x22,
        //           fd=-1, pgoff=0) — sys 192.
        // ebx, ecx, edx, esi, edi, ebp = 6 args.
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]); // mov eax, 192
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (addr=0)
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]); // mov ecx, 0x1000
        out.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]); // mov edx, PROT_R|W
        out.extend_from_slice(&[0xBE, 0x22, 0x00, 0x00, 0x00]); // mov esi, 0x22
        out.extend_from_slice(&[0xBF, 0xFF, 0xFF, 0xFF, 0xFF]); // mov edi, -1
        out.extend_from_slice(&[0x31, 0xED]); // xor ebp, ebp (pgoff=0)
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[buf], eax — stash returned address
        out.push(0xA3);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        // mov esi, eax — also keep in esi for the byte probe
        out.extend_from_slice(&[0x89, 0xC6]);
        // mov byte ptr [esi], 0x42 — write sentinel into the
        // first byte of the new page. If mmap returned -errno
        // (large negative as u32, e.g. 0xFFFFFFF...), this
        // dereferences kernel space and would #PF; tested below.
        out.extend_from_slice(&[0xC6, 0x06, 0x42]);
        // mov al, byte ptr [esi] — read it back
        out.extend_from_slice(&[0x8A, 0x06]);
        // mov ds:[buf_val], al — store the byte
        out.extend_from_slice(&[0xA2]);
        out.extend_from_slice(&buf_val_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_mid, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_mid_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_mid.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf_val, 1)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_val_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_end, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_end_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_end.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_end_addr = marker_mid_addr + marker_mid.len() as u32;
    let buf_addr = marker_end_addr + marker_end.len() as u32;
    let buf_val_addr = buf_addr + 4;
    let code = build_code(
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        buf_addr,
        buf_val_addr,
    );
    let buf_zeros = [0u8; 4];
    let buf_val = [0u8; 1];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_mid, marker_end, &buf_zeros, &buf_val],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates a new file in initramfs,
/// writes 8 bytes to it, closes, reopens read-only, reads the
/// 8 bytes back, and prints them between markers:
///
///   `[USERSPACE FILE=<8 bytes>][USERSPACE END]`
///
/// Sequence:
///
///   fd1 = sys_open("/test_file", O_CREAT|O_WRONLY, 0o644)
///   sys_write(fd1, "TESTDATA", 8)
///   sys_close(fd1)
///   fd2 = sys_open("/test_file", O_RDONLY, 0)
///   sys_read(fd2, buf, 8)
///   sys_close(fd2)
///   ... write markers + buf + exit
///
/// Pins:
///   - writable initramfs (rootfs is tmpfs by default; this
///     verifies the kernel can create + write + read files in
///     the rootfs the cpio unpacker set up)
///   - inode creation through `do_filp_open` with O_CREAT —
///     until now only existing files (/proc/version, /helper)
///     were opened
///   - sys_close: never previously exercised; tests that fd
///     teardown doesn't leak or corrupt
///   - cross-fd persistence: data written through one fd
///     becomes readable through a different fd opened to the
///     same path (tmpfs page cache works)
fn build_initramfs_file_io() -> Vec<u8> {
    let path: &[u8] = b"/test_file\0";
    let payload: &[u8] = b"TESTDATA";
    let marker_pre: &[u8] = b"[USERSPACE FILE=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    // Each fd is saved to memory between syscalls so it can be
    // reused after eax gets clobbered by the next syscall number.
    let build_code = |path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd1_addr: u32,
                      fd2_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd1 = sys_open(path, O_CREAT|O_WRONLY = 0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]); // mov ecx, 0x41
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644 = 0x1A4
        out.extend_from_slice(&[0xCD, 0x80]);
        // ds:[fd1] = eax
        out.push(0xA3);
        out.extend_from_slice(&fd1_addr.to_le_bytes());
        // sys_write(fd1, payload, 8)
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd1]
        out.extend_from_slice(&fd1_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd1)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd1_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]); // mov eax, 6
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd2 = sys_open(path, O_RDONLY = 0, 0)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (O_RDONLY)
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        out.extend_from_slice(&[0xCD, 0x80]);
        // ds:[fd2] = eax
        out.push(0xA3);
        out.extend_from_slice(&fd2_addr.to_le_bytes());
        // sys_read(fd2, buf, 8)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd2_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd2)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd2_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 8)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let payload_addr = path_addr + path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd1_addr = marker_post_addr + marker_post.len() as u32;
    let fd2_addr = fd1_addr + 4;
    let buf_addr = fd2_addr + 4;
    let code = build_code(
        path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd1_addr,
        fd2_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let buf_zeros = [0u8; 8];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            payload,
            marker_pre,
            marker_post,
            &fd_zeros,
            &fd_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates an 8-byte file `/probe`,
/// calls `sys_stat64("/probe", &statbuf)` (syscall 195), and
/// reads `st_size` (low 4 bytes at offset 44 in `struct stat64`
/// on i386) back out. Writes the 4-byte size between
/// `[USERSPACE STAT_SIZE=…][USERSPACE END]`. Test asserts the
/// value equals 8 (the byte count we just wrote).
///
/// Pins:
///   - `sys_stat64`: a syscall that fills a ~96-byte struct in
///     userspace (kernel `cp_new_stat64` → `copy_to_user`). The
///     file-io milestone already proved write/read through fds;
///     stat tests the *inode metadata* path independently
///   - i386 stat64 ABI: `st_size` lives at offset 44 in the
///     struct. If the layout were wrong /init would read garbage
///   - integration between write and stat: the kernel must
///     update the inode's i_size after the write, otherwise
///     stat reports stale 0 even though the bytes are in the
///     page cache
fn build_initramfs_stat() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let payload: &[u8] = b"TESTDATA"; // 8 bytes
    let marker_pre: &[u8] = b"[USERSPACE STAT_SIZE=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      statbuf_addr: u32,
                      size_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open(path, O_CREAT|O_WRONLY=0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]); // mov ecx, 0x41
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        // ds:[fd] = eax
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 8)
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]); // mov eax, 6
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_stat64(path, &statbuf) — sys 195
        out.extend_from_slice(&[0xB8, 0xC3, 0x00, 0x00, 0x00]); // mov eax, 195
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&statbuf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // Read st_size low 4 bytes (offset 44 in struct stat64).
        out.push(0xA1); // mov eax, ds:[statbuf+44]
        out.extend_from_slice(&(statbuf_addr + 44).to_le_bytes());
        // ds:[size_buf] = eax
        out.push(0xA3);
        out.extend_from_slice(&size_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, size_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&size_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let payload_addr = path_addr + path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let statbuf_addr = fd_addr + 4;
    let size_buf_addr = statbuf_addr + 100; // generous: i386 struct stat64 is ~96 bytes
    let code = build_code(
        path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        statbuf_addr,
        size_buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let statbuf_zeros = [0u8; 100];
    let size_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            payload,
            marker_pre,
            marker_post,
            &fd_zeros,
            &statbuf_zeros,
            &size_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates an 8-byte file `/probe`
/// containing "TESTDATA", then `sys_lseek`s the file's fd to
/// offset 4 (SEEK_SET) and reads 4 bytes — should be the
/// substring "DATA" (offsets 4..8 of the original). Writes the
/// 4 bytes between `[USERSPACE LSEEK=…][USERSPACE END]` markers.
/// Test asserts the round-trip equals `b"DATA"`.
///
/// Pins random-access I/O: a real read/write fd has a *position*
/// in the kernel's `struct file`, and `sys_lseek` mutates it.
/// Sequential read at offset 0 (existing milestones) doesn't
/// exercise the position arithmetic; this milestone does.
fn build_initramfs_lseek() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let payload: &[u8] = b"TESTDATA"; // bytes 4..8 = "DATA"
    let marker_pre: &[u8] = b"[USERSPACE LSEEK=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open(path, O_CREAT|O_RDWR=0x42, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x42, 0x00, 0x00, 0x00]); // mov ecx, O_CREAT|O_RDWR
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        // ds:[fd] = eax
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 8)
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_lseek(fd, 4, SEEK_SET=0) — sys 19
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x13, 0x00, 0x00, 0x00]); // mov eax, 19
        out.extend_from_slice(&[0xB9, 0x04, 0x00, 0x00, 0x00]); // mov ecx, 4
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (SEEK_SET)
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_read(fd, buf, 4)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let payload_addr = path_addr + path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let buf_addr = fd_addr + 4;
    let code = build_code(
        path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let buf_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            payload,
            marker_pre,
            marker_post,
            &fd_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init proves `sys_dup2` (syscall 63) works
/// — the foundation primitive shells use for stdin/stdout/stderr
/// redirection. Plan:
///
///   fd_log = open("/log", O_CREAT|O_WRONLY, 0o644)
///   dup2(fd_log, 1)         ; now fd 1 ALSO points to /log
///   write(1, "REDIRECTED", 10)  ; goes to /log, NOT to UART
///   close(fd_log)
///   close(1)                ; closes /log via fd 1
///   fd_read = open("/log", O_RDONLY)   ; gets fd 1 (lowest free)
///   read(fd_read, buf, 10)
///   close(fd_read)
///   write(2, "[USERSPACE DUP2=", 16)   ; fd 2 still /dev/console
///   write(2, buf, 10)
///   write(2, "][USERSPACE END]\n", 17)
///   exit(0)
///
/// Pins:
///   - sys_dup2: kernel must wire the existing `struct file *`
///     into the newfd slot of /init's fd table, releasing
///     whatever was there before
///   - shared file struct: writes through both fd_log and fd 1
///     hit the same backing file (the `f_pos` of /log is shared
///     across both fds — that's the whole point of dup2)
///   - fd 2 == /dev/console: every existing milestone writes
///     through fd 1; this proves the kernel set up fd 2 the
///     same way so the diagnostic markers land in UART
///
/// If dup2 silently failed, write(1, "REDIRECTED", 10) would go
/// to UART directly (unchanged fd 1), the /log file would be
/// empty, and the test would see `[USERSPACE DUP2=][USERSPACE END]`
/// — no 10 bytes between the markers — and fail the assertion.
fn build_initramfs_dup2() -> Vec<u8> {
    let path: &[u8] = b"/log\0";
    let payload: &[u8] = b"REDIRECTED"; // 10 bytes
    let marker_pre: &[u8] = b"[USERSPACE DUP2=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(320);
        // fd_log = sys_open(path, O_CREAT|O_WRONLY=0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]); // mov ecx, 0x41
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[fd], eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_dup2(fd_log, 1) — sys 63
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x3F, 0x00, 0x00, 0x00]); // mov eax, 63
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, 1
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_write(1, payload, 10) — should go to /log via dup2'd fd 1
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x0A, 0x00, 0x00, 0x00]); // mov edx, 10
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd_log)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(1) — closes the dup2-target slot
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd_read = sys_open(path, O_RDONLY=0, 0)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[fd] = eax (reuse slot)
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_read(fd_read, buf, 10)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x0A, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd_read)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(2, marker_pre, len) — fd 2 untouched by dup2 → UART
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x02, 0x00, 0x00, 0x00]); // mov ebx, 2
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(2, buf, 10)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x02, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x0A, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(2, marker_post, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x02, 0x00, 0x00, 0x00]);
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

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let payload_addr = path_addr + path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let buf_addr = fd_addr + 4;
    let code = build_code(
        path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let buf_zeros = [0u8; 10];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            payload,
            marker_pre,
            marker_post,
            &fd_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates `/probe`, calls
/// `sys_unlink("/probe")` (syscall 10), then calls
/// `sys_stat64("/probe", &statbuf)` which MUST fail with
/// `-ENOENT`. /init writes the stat-after-unlink return code
/// between `[USERSPACE UNLINKED_STAT=…][USERSPACE END]`. Test
/// asserts the 4 bytes equal `0xFFFFFFFE` (which is `-2` sign-
/// extended; `-ENOENT == 2`).
///
/// Pins:
///   - `sys_unlink`: kernel must remove the dentry from its
///     parent, decrement the inode's link count, and (since
///     nothing holds it open) free the inode
///   - negative-result handling: until now every milestone's
///     syscalls succeeded. unlink+stat catches a kernel that
///     might silently report success on a missing file
///   - syscall-return-as-errno ABI: `-errno` is returned in
///     eax as a signed-extended negative; the test catches
///     `-2 != 0` and `-2 == 0xFFFFFFFE` as u32 LE
fn build_initramfs_unlink() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let marker_pre: &[u8] = b"[USERSPACE UNLINKED_STAT=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      statbuf_addr: u32,
                      ret_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open(path, O_CREAT|O_WRONLY=0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]); // mov ecx, O_CREAT|O_WRONLY
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[fd], eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_unlink(path) — sys 10
        out.extend_from_slice(&[0xB8, 0x0A, 0x00, 0x00, 0x00]); // mov eax, 10
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_stat64(path, &statbuf) — should now fail with -ENOENT
        out.extend_from_slice(&[0xB8, 0xC3, 0x00, 0x00, 0x00]); // mov eax, 195
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&statbuf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov ds:[ret_buf], eax — save the (expected -ENOENT) return
        out.push(0xA3);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, ret_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_pre_addr = path_addr + path.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let statbuf_addr = fd_addr + 4;
    let ret_buf_addr = statbuf_addr + 100;
    let code = build_code(
        path_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        statbuf_addr,
        ret_buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let statbuf_zeros = [0u8; 100];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            marker_pre,
            marker_post,
            &fd_zeros,
            &statbuf_zeros,
            &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates `/dir` via `sys_mkdir`, then
/// creates `/dir/test` with 5 bytes "INDIR" (open + write +
/// close), reopens for read, and prints the round-tripped bytes
/// between `[USERSPACE FILE_IN_DIR=…][USERSPACE END]`. Pins:
///   - sys_mkdir: kernel allocates a directory inode + dentry
///     attached to the parent (rootfs) dentry
///   - multi-level path resolution: open("/dir/test", ...) walks
///     "dir" first, then "test" inside it. Until now every test
///     used flat paths at /
fn build_initramfs_mkdir() -> Vec<u8> {
    let dir_path: &[u8] = b"/dir\0";
    let file_path: &[u8] = b"/dir/test\0";
    let payload: &[u8] = b"INDIR";
    let marker_pre: &[u8] = b"[USERSPACE FILE_IN_DIR=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |dir_path_addr: u32,
                      file_path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // sys_mkdir(dir_path, 0o755) — sys 39
        out.extend_from_slice(&[0xB8, 0x27, 0x00, 0x00, 0x00]); // mov eax, 39
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0xED, 0x01, 0x00, 0x00]); // mov ecx, 0o755 = 0x1ED
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd = sys_open(file_path, O_CREAT|O_WRONLY=0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&file_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[fd] = eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 5)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x05, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd = sys_open(file_path, O_RDONLY, 0)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&file_path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_read(fd, buf, 5)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x05, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 5)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x05, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let dir_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let file_path_addr = dir_path_addr + dir_path.len() as u32;
    let payload_addr = file_path_addr + file_path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let buf_addr = fd_addr + 4;
    let code = build_code(
        dir_path_addr,
        file_path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let buf_zeros = [0u8; 5];
    let binary = make_init_elf32_safe(
        &code,
        &[
            dir_path,
            file_path,
            payload,
            marker_pre,
            marker_post,
            &fd_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init samples `sys_time(NULL)` before and
/// after `sys_nanosleep(&ts, NULL)` with `ts = {tv_sec=1,
/// tv_nsec=0}`, and writes both time samples between markers.
/// Test asserts `t1 - t0 >= 1` — proves the kernel actually
/// slept the process for at least 1 second of guest wall time.
///
/// Pins:
///   - sys_nanosleep: kernel sets TASK_INTERRUPTIBLE, arms a
///     timer for `req->tv_sec + req->tv_nsec/1e9` and schedules
///     out. When the timer fires, the task is woken and the
///     syscall returns.
///   - timer + scheduler integration: until now every milestone
///     ran to completion in "instant" guest time (no sleeps).
///     This one proves PIT/LAPIC IRQs actually advance jiffies
///     and trigger task wakeups end-to-end.
fn build_initramfs_nanosleep() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE T0=";
    let marker_mid: &[u8] = b"T1=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      ts_addr: u32,
                      t0_buf_addr: u32,
                      t1_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_time(NULL) → eax = time_t
        out.extend_from_slice(&[0xB8, 0x0D, 0x00, 0x00, 0x00]); // mov eax, 13
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[t0_buf], eax
        out.extend_from_slice(&t0_buf_addr.to_le_bytes());
        // sys_nanosleep(&ts, NULL) — sys 162. ts already contains
        // {1, 0} = 1 second since the data segment was built with
        // 0x01 0x00 0x00 0x00 0x00 0x00 0x00 0x00.
        out.extend_from_slice(&[0xB8, 0xA2, 0x00, 0x00, 0x00]); // mov eax, 162
        out.push(0xBB);
        out.extend_from_slice(&ts_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (rem = NULL)
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_time(NULL) → eax = time_t (post-sleep)
        out.extend_from_slice(&[0xB8, 0x0D, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[t1_buf], eax
        out.extend_from_slice(&t1_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, t0_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&t0_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_mid, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_mid_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_mid.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, t1_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&t1_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_end, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_end_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_end.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_end_addr = marker_mid_addr + marker_mid.len() as u32;
    let ts_addr = marker_end_addr + marker_end.len() as u32;
    let t0_buf_addr = ts_addr + 8;
    let t1_buf_addr = t0_buf_addr + 4;
    let code = build_code(
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        ts_addr,
        t0_buf_addr,
        t1_buf_addr,
    );
    // struct timespec { tv_sec=1, tv_nsec=0 } — 8 bytes
    let ts_data: [u8; 8] = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let t0_zeros = [0u8; 4];
    let t1_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            marker_pre, marker_mid, marker_end, &ts_data, &t0_zeros, &t1_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_writev` (syscall 146)
/// with a 2-element `iovec[]` to atomically write two memory
/// regions to stdout in one syscall. /init's data segment
/// contains both strings plus a manually-laid-out iov array
/// where each entry is `{base_ptr (4 bytes), len (4 bytes)}` for
/// an 8-byte struct on i386. Result in UART:
///
///   `[USERSPACE WRITEV=A` (from vec1) + `B][USERSPACE END]\n` (vec2)
///
/// Test asserts the combined string appears. Pins `sys_writev`:
/// the kernel reads the iovec array out of /init's memory, walks
/// each entry, and concatenates their contents into the fd. The
/// existing write milestones only ever passed a single buffer.
fn build_initramfs_writev() -> Vec<u8> {
    let vec1: &[u8] = b"[USERSPACE WRITEV=A"; // 19 bytes
    let vec2: &[u8] = b"B][USERSPACE END]\n"; // 18 bytes

    let build_code = |iov_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        // sys_writev(fd=1, iov=iov_addr, iovcnt=2) — sys 146
        out.extend_from_slice(&[0xB8, 0x92, 0x00, 0x00, 0x00]); // mov eax, 146
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&iov_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x02, 0x00, 0x00, 0x00]); // mov edx, 2
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0).len() as u32;
    let iov_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let vec1_addr = iov_addr + 16; // 2 iovecs × 8 bytes
    let vec2_addr = vec1_addr + vec1.len() as u32;
    let code = build_code(iov_addr);

    // Build iov_array bytes: [{vec1_addr, vec1.len()},
    //                        {vec2_addr, vec2.len()}]
    let mut iov_bytes = Vec::with_capacity(16);
    iov_bytes.extend_from_slice(&vec1_addr.to_le_bytes());
    iov_bytes.extend_from_slice(&(vec1.len() as u32).to_le_bytes());
    iov_bytes.extend_from_slice(&vec2_addr.to_le_bytes());
    iov_bytes.extend_from_slice(&(vec2.len() as u32).to_le_bytes());

    let binary = make_init_elf32_safe(&code, &[&iov_bytes, vec1, vec2], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_mkdir("/sub", 0o755)`,
/// then `sys_chdir("/sub")`, then `sys_getcwd(buf, 32)`. /init
/// writes the first 4 bytes of `buf` (after kernel filled
/// "/sub\0…") between `[USERSPACE CWD=…][USERSPACE END]` markers.
/// Test asserts the 4 bytes equal `b"/sub"`. Pins:
///   - sys_chdir: kernel updates the current task's `fs->pwd`
///     to point at the new dentry
///   - sys_getcwd: kernel walks the dentry tree from the
///     current `fs->pwd` up to its mount root and assembles a
///     pathname string into the user buffer (different mechanism
///     than read/stat — it's a kernel-side string allocation
///     and copy_to_user)
fn build_initramfs_chdir() -> Vec<u8> {
    let dir_path: &[u8] = b"/sub\0";
    let marker_pre: &[u8] = b"[USERSPACE CWD=";
    let marker_ret: &[u8] = b" RET=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |dir_path_addr: u32,
                      marker_pre_addr: u32,
                      marker_ret_addr: u32,
                      marker_post_addr: u32,
                      buf_addr: u32,
                      ret_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_mkdir(dir_path, 0o755) — sys 39
        out.extend_from_slice(&[0xB8, 0x27, 0x00, 0x00, 0x00]); // mov eax, 39
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0xED, 0x01, 0x00, 0x00]); // mov ecx, 0o755
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_chdir(dir_path) — sys 12
        out.extend_from_slice(&[0xB8, 0x0C, 0x00, 0x00, 0x00]); // mov eax, 12
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_getcwd(buf, 32) — sys 183
        out.extend_from_slice(&[0xB8, 0xB7, 0x00, 0x00, 0x00]); // mov eax, 183
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x20, 0x00, 0x00, 0x00]); // mov ecx, 32
        out.extend_from_slice(&[0xCD, 0x80]);
        // Save getcwd return code so we can see what it returned
        out.push(0xA3);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 4) — first 4 bytes of cwd ("/sub")
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_ret, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_ret_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_ret.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, ret_buf, 4) — getcwd's eax return
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
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

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let dir_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_pre_addr = dir_path_addr + dir_path.len() as u32;
    let marker_ret_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_post_addr = marker_ret_addr + marker_ret.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let ret_buf_addr = buf_addr + 32;
    let code = build_code(
        dir_path_addr,
        marker_pre_addr,
        marker_ret_addr,
        marker_post_addr,
        buf_addr,
        ret_buf_addr,
    );
    let buf_zeros = [0u8; 32];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            dir_path,
            marker_pre,
            marker_ret,
            marker_post,
            &buf_zeros,
            &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Diagnostic /init for the prior `sys_pipe`-broken blocker
/// investigation: calls `sys_pipe2(&fds, 0)` then writes eax +
/// fd slots without touching the pipe further. Kept around as
/// the canonical "what did sys_pipe2 actually return" probe.
fn build_initramfs_pipe_diag() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE PIPE_RET=";
    let marker_fd0: &[u8] = b" FD0=";
    let marker_fd1: &[u8] = b" FD1=";
    let marker_end: &[u8] = b" END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_fd0_addr: u32,
                      marker_fd1_addr: u32,
                      marker_end_addr: u32,
                      fds_addr: u32,
                      ret_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_pipe2(&fds, 0) — syscall 331
        out.extend_from_slice(&[0xB8, 0x4B, 0x01, 0x00, 0x00]); // mov eax, 331
        out.push(0xBB);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0xCD, 0x80]);
        // ds:[ret_buf] = eax (signed errno or 0)
        out.push(0xA3);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, ret_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_fd0, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_fd0_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_fd0.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, fds_addr, 4) — fds[0]
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_fd1, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_fd1_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_fd1.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, fds_addr+4, 4) — fds[1]
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&(fds_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_end, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_end_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_end.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_fd0_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_fd1_addr = marker_fd0_addr + marker_fd0.len() as u32;
    let marker_end_addr = marker_fd1_addr + marker_fd1.len() as u32;
    let fds_addr = marker_end_addr + marker_end.len() as u32;
    let ret_buf_addr = fds_addr + 8;
    let code = build_code(
        marker_pre_addr,
        marker_fd0_addr,
        marker_fd1_addr,
        marker_end_addr,
        fds_addr,
        ret_buf_addr,
    );
    let fds_zeros = [0u8; 8];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            marker_pre, marker_fd0, marker_fd1, marker_end, &fds_zeros, &ret_zeros,
        ],
        7,
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
                              // make_init_elf32_safe auto-pads if the result lands at a
                              // known-bad size (this builder's raw output is exactly 213
                              // bytes — see the discovery in commit e86ed9a). With the
                              // safe wrapper the binary comes out at 217 (213 + 4-byte
                              // pad), which is currently confirmed-good.
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_post, &buf_zeros],
        7, // R | W | X — kernel copy_to_user writes timeval into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_fork` (syscall 2) and then
/// — in BOTH parent and child — writes `[USERSPACE FORK ret=`
/// followed by its own 4-byte `eax`, then `][USERSPACE END]\n`.
/// The marker_fork write happens AFTER the fork on purpose so
/// both processes emit it; the buf and the rest of the post-fork
/// code run in two independent address spaces (kernel
/// copy-on-write), so each process prints its own eax value:
///
///   parent eax = child PID  (>= 2, typically the next free pid)
///   child  eax = 0          (fork(2) contract for the child)
///
/// /init code structure:
///
///   sys_fork           ; eax = child PID (parent) or 0 (child)
///   write(1, marker_fork, len)
///   mov ds:[buf], eax
///   write(1, buf, 4)
///   write(1, marker_end, len)
///   exit(0)
///
/// What this pins beyond `getpid`:
///   - kernel process creation (do_fork → copy_process →
///     copy_mm CoW + new task_struct + add to runqueue)
///   - scheduler actually schedules a child (without that, child's
///     code never runs and only ONE marker shows up in UART)
///   - return-to-user from fork in child with eax=0
fn build_initramfs_fork() -> Vec<u8> {
    let marker_fork: &[u8] = b"[USERSPACE FORK ret=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_fork_addr: u32, marker_end_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // sys_fork() — sys 2. No args. Returns child PID in eax for
        // parent, 0 for child. After this point both processes run
        // independently in CoW-copied address spaces.
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2
        out.extend_from_slice(&[0xCD, 0x80]);
        // Stash fork return in buf BEFORE the waitpid call below
        // clobbers eax — both branches need the original return
        // for the buf write further down.
        out.push(0xA3); // mov ds:[buf_addr], eax
        out.extend_from_slice(&buf_addr.to_le_bytes());
        // sys_waitpid(-1, NULL, 0) — sys 7. Ordering trick: parent
        // blocks until child exits; child returns -ECHILD instantly
        // (no children of its own). So by the time control reaches
        // the writes below, the child has ALREADY run them and
        // exited — parent's writes come second, and only THEN does
        // PID 1 exit and trigger the "Attempted to kill init"
        // panic. Without this, the first parent's `exit(0)` panics
        // the kernel mid-child-write and we see only ONE complete
        // marker_fork + buf + marker_end sequence (verified in
        // /tmp/wwwvm-userspace-fork-second-failure.bin: parent's
        // 0x4B = 75 = child PID landed but child's `0` never did).
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]); // mov eax, 7
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]); // mov ebx, -1
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (status=NULL)
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (options=0)
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_fork, marker_fork.len())
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9); // mov ecx, marker_fork_addr
        out.extend_from_slice(&marker_fork_addr.to_le_bytes());
        out.push(0xBA); // mov edx, len
        out.extend_from_slice(&(marker_fork.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf, 4) — buf already holds the original fork
        // return that we stashed before clobbering eax.
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_end, marker_end.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_end_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_end.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_fork_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_end_addr = marker_fork_addr + marker_fork.len() as u32;
    let buf_addr = marker_end_addr + marker_end.len() as u32;
    let code = build_code(marker_fork_addr, marker_end_addr, buf_addr);
    let buf_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_fork, marker_end, &buf_zeros],
        7, // R | W | X — /init writes fork return into buf
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio with TWO executables: /init (forks + has child
/// execve /helper) and /helper (writes marker, exits). Proves
/// the kernel's path-resolving execve(2) path: child must
/// `do_execve("/helper", NULL, NULL)`, kernel must look /helper
/// up in initramfs, read its ELF header, set up a new mm, jump
/// to /helper's entry point, and the new process must reach
/// userspace and successfully write its marker to UART.
///
/// /init layout:
///
///   sys_fork
///   test eax, eax
///   jnz parent           ; eax != 0 → parent path
///   ; child: execve("/helper", NULL, NULL)
///   sys_execve
///   ; only reached on execve FAILURE (e.g. -ENOENT, -ENOEXEC).
///   ; If we get here, write a distinct error marker so the test
///   ; sees the *cause* instead of just a missing OK marker.
///   write(1, "[USERSPACE EXECVE_FAILED]\n", N)
///   exit(1)
/// parent:
///   sys_waitpid(-1, NULL, 0)
///   exit(0)
///
/// /helper layout (minimal):
///
///   write(1, "[USERSPACE EXECVE_OK]\n", N)
///   exit(0)
fn build_initramfs_execve() -> Vec<u8> {
    let helper_path: &[u8] = b"/helper\0";
    let marker_failed: &[u8] = b"[USERSPACE EXECVE_FAILED]\n";

    let init_build_code =
        |helper_path_addr: u32, marker_failed_addr: u32, _padding: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(96);
            // sys_fork
            out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2
            out.extend_from_slice(&[0xCD, 0x80]);
            // test eax, eax
            out.extend_from_slice(&[0x85, 0xC0]);
            // jnz parent (forward, disp8 — child path follows;
            // parent path is at the end of the function. We'll
            // compute the disp after assembling the child block.)
            // For now reserve 2 bytes; patch after.
            let jnz_at = out.len();
            out.extend_from_slice(&[0x75, 0x00]); // jnz +disp (placeholder)

            let _child_start = out.len();
            // child: sys_execve(helper_path, NULL, NULL) — sys 11.
            out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
            out.push(0xBB);
            out.extend_from_slice(&helper_path_addr.to_le_bytes()); // ebx = filename
            out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (argv=NULL)
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (envp=NULL)
            out.extend_from_slice(&[0xCD, 0x80]);
            // Only reached on execve FAILURE. Print a marker and
            // exit(1) so the test sees the cause.
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&marker_failed_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&(marker_failed.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // exit(1)
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.extend_from_slice(&[0xCD, 0x80]);

            let parent_start = out.len();
            // Patch the jnz disp8.
            let disp = (parent_start - (jnz_at + 2)) as i32;
            assert!(
                (-128..=127).contains(&disp),
                "child block too large for jnz disp8 (got {disp}); switch to jnz rel32"
            );
            out[jnz_at + 1] = disp as u8;

            // parent: sys_waitpid(-1, NULL, 0)
            out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]); // mov eax, 7
            out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]); // mov ebx, -1
            out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
            out.extend_from_slice(&[0xCD, 0x80]);
            // exit(0)
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };

    let init_code_len = init_build_code(0, 0, 0).len() as u32;
    let helper_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + init_code_len;
    let marker_failed_addr = helper_path_addr + helper_path.len() as u32;
    let init_code = init_build_code(helper_path_addr, marker_failed_addr, 0);
    let init_binary = make_init_elf32_safe(&init_code, &[helper_path, marker_failed], 5);

    // /helper — minimal "write marker + exit" binary.
    let marker_ok: &[u8] = b"[USERSPACE EXECVE_OK]\n";
    let helper_build_code = |marker_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(40);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_ok.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let helper_code_len = helper_build_code(0).len() as u32;
    let helper_marker_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + helper_code_len;
    let helper_code = helper_build_code(helper_marker_addr);
    let helper_binary = make_init_elf32_safe(&helper_code, &[marker_ok], 5);

    build_cpio_archive_with_helper(&init_binary, "helper", &helper_binary)
}

/// Build a cpio whose /init execve's /helper with a real argv
/// `["/helper", "ARG1"]`, and /helper reads `argv[1]` off its
/// own stack at entry (i386 SysV: `[esp+0] = argc`, `[esp+4] =
/// argv[0]`, `[esp+8] = argv[1]`) and writes the pointed-to
/// string out: `[USERSPACE ARGV1=ARG1][USERSPACE END]`.
///
/// What this pins beyond the basic execve milestone:
///   - kernel `copy_strings` during execve: argv pointers and
///     strings are walked in /init's old userspace, packed into
///     a kernel buffer, and re-laid-out on the NEW process's
///     stack (the existing execve test passes argv=NULL, so the
///     copy_strings path is skipped entirely)
///   - process-startup ABI: kernel sets up argc/argv/envp/auxv
///     in the standard layout at the new process's esp, and
///     userspace can read them through plain mov
///
/// /init data layout (addresses computed at build time):
///   argv_arr (12 bytes) — [helper_path_addr, arg1_addr, 0]
///   helper_path "/helper\0"
///   arg1 "ARG1\0"
///   fail_marker (printed only if execve returns to child)
fn build_initramfs_argv() -> Vec<u8> {
    let helper_path: &[u8] = b"/helper\0";
    let arg1: &[u8] = b"ARG1\0";
    let init_fail_marker: &[u8] = b"[USERSPACE EXECVE_FAILED]\n";

    // /init code: fork → child execve("/helper", argv_arr, NULL);
    // on failure write fail_marker and exit(1). Parent waitpids.
    let init_build_code = |helper_path_addr: u32, argv_arr_addr: u32, fail_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        // sys_fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2
        out.extend_from_slice(&[0xCD, 0x80]);
        // test eax, eax; jnz parent (disp8 patched)
        out.extend_from_slice(&[0x85, 0xC0]);
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]);
        // child: sys_execve(helper_path, argv_arr, NULL)
        out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
        out.push(0xBB);
        out.extend_from_slice(&helper_path_addr.to_le_bytes()); // ebx
        out.push(0xB9);
        out.extend_from_slice(&argv_arr_addr.to_le_bytes()); // ecx = argv
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (envp = NULL)
        out.extend_from_slice(&[0xCD, 0x80]);
        // execve returned → failure. Write fail_marker, exit(1).
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&fail_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(init_fail_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);

        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "init's child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;

        // parent: sys_waitpid(-1, NULL, 0); exit(0)
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let init_code_len = init_build_code(0, 0, 0).len() as u32;
    let argv_arr_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + init_code_len;
    let helper_path_addr = argv_arr_addr + 12; // 3 pointers * 4 bytes
    let arg1_addr = helper_path_addr + helper_path.len() as u32;
    let fail_addr = arg1_addr + arg1.len() as u32;

    // argv_arr: [helper_path_addr, arg1_addr, NULL]
    let mut argv_arr = Vec::with_capacity(12);
    argv_arr.extend_from_slice(&helper_path_addr.to_le_bytes());
    argv_arr.extend_from_slice(&arg1_addr.to_le_bytes());
    argv_arr.extend_from_slice(&0u32.to_le_bytes());

    let init_code = init_build_code(helper_path_addr, argv_arr_addr, fail_addr);
    let init_binary = make_init_elf32_safe(
        &init_code,
        &[&argv_arr, helper_path, arg1, init_fail_marker],
        5,
    );

    // /helper: reads argv[1] off stack ([esp+8]), writes the 4
    // bytes "ARG1" pointed to between markers. We assume argv[1]
    // is 4 chars + NUL; for this test the value is fixed at
    // build time so write length = 4 is fine. A real getenv-
    // style reader would call strlen first.
    let helper_marker_pre: &[u8] = b"[USERSPACE ARGV1=";
    let helper_marker_post: &[u8] = b"][USERSPACE END]\n";
    let helper_build_code = |pre_addr: u32, post_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(80);
        // mov esi, [esp+8] — cache argv[1] pointer. esi survives
        // i386 syscall round-trips (kernel restores GP regs on
        // sysret-ish path), so no need to spill to memory.
        out.extend_from_slice(&[0x8B, 0x74, 0x24, 0x08]);
        // write(1, marker_pre, marker_pre.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(helper_marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, argv[1], 4) — restore ecx from esi.
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x89, 0xF1]); // mov ecx, esi
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, marker_post.len())
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(helper_marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let helper_code_len = helper_build_code(0, 0).len() as u32;
    let helper_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + helper_code_len;
    let helper_post_addr = helper_pre_addr + helper_marker_pre.len() as u32;
    let helper_code = helper_build_code(helper_pre_addr, helper_post_addr);
    let helper_binary =
        make_init_elf32_safe(&helper_code, &[helper_marker_pre, helper_marker_post], 5);

    build_cpio_archive_with_helper(&init_binary, "helper", &helper_binary)
}

/// Build a cpio whose /init execve's /helper with a real envp
/// `["KEY=VAL"]` (and argv `["/helper"]`); /helper reads
/// `envp[0]` off its stack and writes the pointed-to 7 bytes
/// between `[USERSPACE ENV=…][USERSPACE END]` markers. Pins
/// the same `copy_strings` path the argv milestone pins but
/// for the envp half (execve walks BOTH argv and envp arrays;
/// argv test alone doesn't prove envp), plus the i386 SysV
/// process-startup stack layout after argv terminator: with
/// argc=1, `[esp+0]=argc, [esp+4]=argv[0], [esp+8]=NULL, [esp+12]=envp[0]`.
///
/// Why argc=1 (not 2 like argv test): keeps the stack offset
/// deterministic so /helper can hardcode `[esp+0x0C]`. A
/// variable-argc envp reader would need to walk argv until
/// it hits NULL — separate test if we ever want it.
fn build_initramfs_envp() -> Vec<u8> {
    let helper_path: &[u8] = b"/helper\0";
    let env_var: &[u8] = b"KEY=VAL\0";
    let init_fail_marker: &[u8] = b"[USERSPACE EXECVE_FAILED]\n";

    // /init code: fork → child execve("/helper", argv, envp);
    // fail path same as argv milestone.
    let init_build_code = |helper_path_addr: u32,
                           argv_arr_addr: u32,
                           envp_arr_addr: u32,
                           fail_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        // sys_fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0x85, 0xC0]);
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]);
        // child: sys_execve(helper_path, argv_arr, envp_arr)
        out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
        out.push(0xBB);
        out.extend_from_slice(&helper_path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&argv_arr_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&envp_arr_addr.to_le_bytes()); // envp now real
        out.extend_from_slice(&[0xCD, 0x80]);
        // fail path
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&fail_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(init_fail_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);

        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "init's child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;

        // parent: waitpid + exit
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let init_code_len = init_build_code(0, 0, 0, 0).len() as u32;
    // Data layout: argv_arr (8 bytes — 2 pointers including NULL terminator),
    // envp_arr (8 bytes), helper_path (8), env_var (8), fail_marker.
    let argv_arr_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + init_code_len;
    let envp_arr_addr = argv_arr_addr + 8;
    let helper_path_addr = envp_arr_addr + 8;
    let env_var_addr = helper_path_addr + helper_path.len() as u32;
    let fail_addr = env_var_addr + env_var.len() as u32;

    let mut argv_arr = Vec::with_capacity(8);
    argv_arr.extend_from_slice(&helper_path_addr.to_le_bytes());
    argv_arr.extend_from_slice(&0u32.to_le_bytes()); // NULL terminator

    let mut envp_arr = Vec::with_capacity(8);
    envp_arr.extend_from_slice(&env_var_addr.to_le_bytes());
    envp_arr.extend_from_slice(&0u32.to_le_bytes()); // NULL terminator

    let init_code = init_build_code(helper_path_addr, argv_arr_addr, envp_arr_addr, fail_addr);
    let init_binary = make_init_elf32_safe(
        &init_code,
        &[&argv_arr, &envp_arr, helper_path, env_var, init_fail_marker],
        5,
    );

    // /helper: read envp[0] from [esp+0x0C], write 7 bytes
    // ("KEY=VAL") bracketed. esi survives syscall round-trips.
    let helper_marker_pre: &[u8] = b"[USERSPACE ENV=";
    let helper_marker_post: &[u8] = b"][USERSPACE END]\n";
    let helper_build_code = |pre_addr: u32, post_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(80);
        // mov esi, [esp+0x0C] — with argc=1, envp[0] is at
        // [esp+12]: [esp+0]=argc, [esp+4]=argv[0], [esp+8]=NULL
        // (argv terminator), [esp+12]=envp[0]
        out.extend_from_slice(&[0x8B, 0x74, 0x24, 0x0C]);
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(helper_marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, envp[0], 7) — 7 = strlen("KEY=VAL")
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x89, 0xF1]); // mov ecx, esi
        out.extend_from_slice(&[0xBA, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, marker_post, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&post_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(helper_marker_post.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let helper_code_len = helper_build_code(0, 0).len() as u32;
    let helper_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + helper_code_len;
    let helper_post_addr = helper_pre_addr + helper_marker_pre.len() as u32;
    let helper_code = helper_build_code(helper_pre_addr, helper_post_addr);
    let helper_binary =
        make_init_elf32_safe(&helper_code, &[helper_marker_pre, helper_marker_post], 5);

    build_cpio_archive_with_helper(&init_binary, "helper", &helper_binary)
}

/// Build a cpio with THREE executables — /init, /h1, /h2 — so
/// /init forks, child execve's /h1, /h1 execve's /h2, /h2 writes
/// the OK marker and exits. The extra hop over the basic execve
/// milestone pins:
///
///   - execve called from a *non-PID-1* process (the forked child
///     is PID 2+; existing execve milestone covered child→helper,
///     but child is still the same process as the forked one; here
///     /h1 became a new process via execve and *that* /h1 has to
///     execve again — proves execve doesn't have a one-shot
///     post-fork-only fast path)
///   - process-image swap from an *already-exec'd* image, not
///     from the original fork (the kernel has to tear down /h1's
///     mm and set up /h2's, exactly like the first swap from the
///     forked /init image to /h1)
///   - VFS lookup still works after the first execve (path
///     resolution from "/h2" through initramfs root)
///
/// Each stage has its own distinct FAILED marker, so a failure
/// at any stage tells the test exactly which level broke (rather
/// than just "OK didn't appear"):
///
///   /init's child writes `[USERSPACE H1_EXEC_FAILED]` if its
///     execve("/h1", ...) returns
///   /h1 writes `[USERSPACE H2_EXEC_FAILED]` if its
///     execve("/h2", ...) returns
///   /h2 writes `[USERSPACE H2_OK]\n` — the success marker
fn build_initramfs_execve_chain() -> Vec<u8> {
    // Shared shape for /init's child path + /h1: an ELF that
    // execve's a path and writes a marker on failure.
    let make_execer = |target_path: &[u8], fail_marker: &[u8]| -> Vec<u8> {
        let build_code = |target_addr: u32, fail_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(64);
            // sys_execve(target, NULL, NULL) — sys 11.
            out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
            out.push(0xBB);
            out.extend_from_slice(&target_addr.to_le_bytes());
            out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
            out.extend_from_slice(&[0xCD, 0x80]);
            // Only reached if execve FAILED. Write fail marker.
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&fail_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&(fail_marker.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // exit(1)
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };
        let code_len = build_code(0, 0).len() as u32;
        let target_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
        let fail_addr = target_addr + target_path.len() as u32;
        let code = build_code(target_addr, fail_addr);
        make_init_elf32_safe(&code, &[target_path, fail_marker], 5)
    };

    // /h2 — leaf: just writes OK and exits.
    let h2_marker: &[u8] = b"[USERSPACE H2_OK]\n";
    let h2_build_code = |marker_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(40);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(h2_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let h2_code_len = h2_build_code(0).len() as u32;
    let h2_marker_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + h2_code_len;
    let h2_code = h2_build_code(h2_marker_addr);
    let h2_binary = make_init_elf32_safe(&h2_code, &[h2_marker], 5);

    // /h1 — exec'er: execve("/h2", NULL, NULL).
    let h1_binary = make_execer(b"/h2\0", b"[USERSPACE H2_EXEC_FAILED]\n");

    // /init — forks; child execve's /h1; parent waitpids and exits.
    let init_fail_marker: &[u8] = b"[USERSPACE H1_EXEC_FAILED]\n";
    let h1_path: &[u8] = b"/h1\0";
    let init_build_code = |h1_path_addr: u32, fail_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(96);
        // sys_fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2
        out.extend_from_slice(&[0xCD, 0x80]);
        // test eax, eax; jnz parent (disp8 to be patched)
        out.extend_from_slice(&[0x85, 0xC0]);
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]);

        // child: execve("/h1", NULL, NULL)
        out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
        out.push(0xBB);
        out.extend_from_slice(&h1_path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        out.extend_from_slice(&[0xCD, 0x80]);
        // only reached on failure: write fail marker
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&fail_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(init_fail_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(1)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);

        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "init's child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;

        // parent: sys_waitpid(-1, NULL, 0); exit(0)
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let init_code_len = init_build_code(0, 0).len() as u32;
    let h1_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + init_code_len;
    let fail_addr = h1_path_addr + h1_path.len() as u32;
    let init_code = init_build_code(h1_path_addr, fail_addr);
    let init_binary = make_init_elf32_safe(&init_code, &[h1_path, init_fail_marker], 5);

    build_cpio_archive_with_two_helpers(&init_binary, "h1", &h1_binary, "h2", &h2_binary)
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

/// Process-creation milestone: /init calls `sys_fork` (syscall 2),
/// then both parent AND child run the rest of /init's code and
/// each emit `[USERSPACE FORK ret=<eax_le32>][USERSPACE END]`.
/// Parent's eax is the child PID (>= 2), child's eax is 0. The
/// test searches `cumulative` for both occurrences, decodes their
/// 4-byte return values, asserts:
///
///   - one is exactly 0 (child fork return)
///   - the other is >= 2 and < 0x80000000 (sensible child PID,
///     not a sign-extended -errno from a failed fork)
///   - the two values differ (parent saw the child it spawned,
///     child saw the contract'd 0)
///
/// What this pins beyond every earlier milestone:
///   - kernel process creation: do_fork → copy_process →
///     copy_mm (CoW page tables) + dup_task_struct +
///     wake_up_new_task
///   - scheduler actually running both processes: if the child
///     never gets cycles, only ONE marker appears in UART and
///     the test fails the "two occurrences" check
///   - return-to-user-from-fork in the child: kernel must
///     set up child's regs so it re-enters userspace at the
///     same EIP with eax=0, *into its own address space*
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_fork_milestone() {
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
    let cpio = build_initramfs_fork();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_fork: &[u8] = b"[USERSPACE FORK ret=";
    let marker_end_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();

    // Run until we see TWO end markers (parent + child both
    // finished). One run_until_marker gets us the first; we then
    // keep stepping and re-checking for a SECOND occurrence.
    let r1 = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "first `[USERSPACE END]` not seen in 16 B steps — fork may have failed or \
                 child never ran; {}",
                dump_uart_on_failure(&cumulative, "fork-first")
            )
        });
    eprintln!("first end marker after {r1} steps");

    // Step further until a SECOND occurrence appears. We re-scan
    // cumulative from offset just past the first END's position
    // each chunk; budget another 4 B for the second one.
    let chunk = 10_000_000u32;
    let mut extra_steps = 0u64;
    let extra_budget = 4_000_000_000u64;
    let second_pos = loop {
        // Find all occurrences in cumulative; we need at least two.
        let mut positions = cumulative
            .windows(marker_end_search.len())
            .enumerate()
            .filter(|(_, w)| *w == marker_end_search)
            .map(|(i, _)| i);
        let _first = positions.next(); // already-found
        if let Some(p) = positions.next() {
            break p;
        }
        if extra_steps >= extra_budget {
            panic!(
                "second `[USERSPACE END]` not seen +{extra_budget} steps after first — child \
                 likely never executed; {}",
                dump_uart_on_failure(&cumulative, "fork-second")
            );
        }
        let (s, _) = vm.run_steps_idle_aware(chunk);
        extra_steps += s as u64;
        if extra_steps % 100_000_000 < chunk as u64 {
            let out = vm.drain_output();
            if !out.is_empty() {
                cumulative.extend_from_slice(&out);
            }
        }
    };
    eprintln!("second end marker after +{extra_steps} more steps (offset {second_pos})");

    // Find all `[USERSPACE FORK ret=` occurrences; extract the
    // 4-byte eax that follows each.
    let fork_positions: Vec<usize> = cumulative
        .windows(marker_fork.len())
        .enumerate()
        .filter(|(_, w)| *w == marker_fork)
        .map(|(i, _)| i)
        .collect();
    assert!(
        fork_positions.len() >= 2,
        "expected >= 2 `[USERSPACE FORK ret=` occurrences, got {}; {}",
        fork_positions.len(),
        dump_uart_on_failure(&cumulative, "fork-count")
    );
    let mut eaxes: Vec<u32> = fork_positions
        .iter()
        .take(2)
        .map(|&p| {
            let off = p + marker_fork.len();
            let b: [u8; 4] = cumulative[off..off + 4]
                .try_into()
                .expect("4 bytes after marker_fork");
            u32::from_le_bytes(b)
        })
        .collect();
    eaxes.sort();
    eprintln!("fork returns observed: {:?}", &eaxes);
    let child_return = eaxes[0];
    let parent_return = eaxes[1];
    assert_eq!(
        child_return,
        0,
        "expected child's fork return = 0, got {child_return}; {}",
        dump_uart_on_failure(&cumulative, "fork-child-nonzero")
    );
    assert!(
        (2..0x8000_0000).contains(&parent_return),
        "expected parent's fork return in [2, 0x80000000), got {parent_return} (0x{parent_return:08X}); {}",
        dump_uart_on_failure(&cumulative, "fork-parent-bad")
    );
}

/// Execve milestone: /init forks, child calls
/// `sys_execve("/helper", NULL, NULL)`, /helper writes
/// `[USERSPACE EXECVE_OK]` and exits. Parent waitpid's. Test
/// asserts the OK marker appears (and the FAILED marker does
/// not). Pins the kernel's *second-binary* loading path: path
/// resolve in initramfs, ELF parse, address-space teardown,
/// new mm setup, jump to /helper's entry point — none of which
/// the boot-time initramfs-unpack path exercises in the same
/// way (boot exec is a kernel-internal call, not user-issued
/// `execve(2)` from a forked child).
///
/// Two diagnostic markers: OK if execve succeeded + helper
/// ran; FAILED (`[USERSPACE EXECVE_FAILED]`) if execve
/// returned to the caller (which only happens on failure —
/// success replaces the process image entirely). If the test
/// fails, the dump tells us *which* mode it failed in.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_execve_milestone() {
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
    let cpio = build_initramfs_execve();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_ok: &[u8] = b"[USERSPACE EXECVE_OK]";
    let marker_failed: &[u8] = b"[USERSPACE EXECVE_FAILED]";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_ok, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            let cause = if cumulative
                .windows(marker_failed.len())
                .any(|w| w == marker_failed)
            {
                "execve returned to child — likely -ENOENT (path not in initramfs?) \
                 or -ENOEXEC (bad ELF)"
            } else {
                "neither OK nor FAILED marker seen — fork may have failed before execve, \
                 or the parent's waitpid-then-exit raced ahead of child"
            };
            panic!(
                "`[USERSPACE EXECVE_OK]` not seen in 16 B steps; {cause}; {}",
                dump_uart_on_failure(&cumulative, "execve")
            )
        });
    eprintln!("execve OK marker seen after {steps} steps");
    assert!(
        !cumulative
            .windows(marker_failed.len())
            .any(|w| w == marker_failed),
        "saw FAILED marker too — both branches ran, which means execve happened \
         AND then somehow returned. Shouldn't be possible; {}",
        dump_uart_on_failure(&cumulative, "execve-both")
    );
}

/// Execve-chain milestone: /init → fork → child execve("/h1") →
/// /h1 execve("/h2") → /h2 writes `[USERSPACE H2_OK]` and exits.
/// Two distinct execve-FAILED markers tell the test exactly
/// which hop broke. Pins execve called from a process that was
/// *itself* started via execve (proves it's not a one-shot
/// post-fork-only fast path).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_execve_chain_milestone() {
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
    let cpio = build_initramfs_execve_chain();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_ok: &[u8] = b"[USERSPACE H2_OK]";
    let marker_h1_failed: &[u8] = b"[USERSPACE H1_EXEC_FAILED]";
    let marker_h2_failed: &[u8] = b"[USERSPACE H2_EXEC_FAILED]";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_ok, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            let cause = if cumulative
                .windows(marker_h1_failed.len())
                .any(|w| w == marker_h1_failed)
            {
                "/init's child saw execve(\"/h1\") return — first hop broke (path lookup? \
                 mm setup?)"
            } else if cumulative
                .windows(marker_h2_failed.len())
                .any(|w| w == marker_h2_failed)
            {
                "/h1 saw execve(\"/h2\") return — second hop broke (execve from \
                 already-exec'd image fails)"
            } else {
                "no marker — fork may have failed, or first execve hung before either \
                 returning or reaching /h1's userspace"
            };
            panic!(
                "`[USERSPACE H2_OK]` not seen in 16 B steps; {cause}; {}",
                dump_uart_on_failure(&cumulative, "execve-chain")
            )
        });
    eprintln!("H2_OK marker seen after {steps} steps");
    // Neither fail marker should be present. The whole point of
    // the chain is that BOTH execve's succeed.
    assert!(
        !cumulative
            .windows(marker_h1_failed.len())
            .any(|w| w == marker_h1_failed),
        "saw H1_EXEC_FAILED but also OK — impossible without two children; {}",
        dump_uart_on_failure(&cumulative, "execve-chain-h1-fail")
    );
    assert!(
        !cumulative
            .windows(marker_h2_failed.len())
            .any(|w| w == marker_h2_failed),
        "saw H2_EXEC_FAILED but also OK — impossible without two children; {}",
        dump_uart_on_failure(&cumulative, "execve-chain-h2-fail")
    );
}

/// Process-memory-layout milestone: /init calls `sys_brk(0)`
/// (syscall 45, ebx=0 query), and the test asserts the returned
/// program break is at or above /init's binary end. Same shape
/// as the time/getpid milestones (4-byte buffer between markers)
/// but pins a different kernel subsystem: the per-process
/// `mm->brk` is initialized to the end of the ELF data segment,
/// rounded up to a page. Any value below /init's binary end
/// would mean the kernel didn't set up the heap pointer at all
/// — anything finer than "page-aligned at or above" we leave
/// to a future heap-extend milestone.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_brk_milestone() {
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
    let cpio = build_initramfs_brk();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE BRK=";
    let marker_post_search: &[u8] = b"]\r\n[USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "brk")
            )
        });
    eprintln!("sys_brk userspace milestone seen after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let b_off = pre_pos + marker_pre.len();
    let b_bytes: [u8; 4] = cumulative[b_off..b_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let brk = u32::from_le_bytes(b_bytes);
    eprintln!("sys_brk returned brk = 0x{brk:08X}");

    // /init loads at INIT_LOAD_ADDR (0x08048000). The kernel sets
    // mm->brk to the end of the bss/data segment, rounded up to
    // a page (4 KiB on i386). So brk must be at or above
    // LOAD_ADDR + binary_len, and below the kernel split
    // (0xC0000000 on stock i386).
    const KERNEL_BASE: u32 = 0xC000_0000;
    assert!(
        brk >= INIT_LOAD_ADDR,
        "sys_brk returned 0x{brk:08X} (below /init's load address 0x{INIT_LOAD_ADDR:08X}); {}",
        dump_uart_on_failure(&cumulative, "brk-low")
    );
    assert!(
        brk < KERNEL_BASE,
        "sys_brk returned 0x{brk:08X} (above kernel base 0x{KERNEL_BASE:08X}); {}",
        dump_uart_on_failure(&cumulative, "brk-high")
    );
    assert_eq!(
        brk & 0xFFF,
        0,
        "sys_brk returned 0x{brk:08X} — not page-aligned (low 12 bits = {:#X}); {}",
        brk & 0xFFF,
        dump_uart_on_failure(&cumulative, "brk-unaligned")
    );
}

/// Heap-extend milestone: /init queries the current break with
/// `sys_brk(0)`, requests `current + 0x1000`, and the test
/// asserts the new break equals `old + 0x1000` exactly.
///
/// What this pins beyond the brk-query milestone:
///   - on-demand page allocation: the kernel actually maps a
///     new page when brk is extended, not just bumps the
///     pointer
///   - brk's set semantics (vs query): brk(2) returns the new
///     break on success OR the current (unchanged) break on
///     failure; if heap growth was rejected the test catches
///     `new == old` instead of `new == old + 0x1000`
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_brk_extend_milestone() {
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
    let cpio = build_initramfs_brk_extend();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE BRK_OLD=";
    let marker_mid: &[u8] = b"BRK_NEW=";
    let marker_end_search: &[u8] = b"END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`END]` marker not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "brk-extend")
            )
        });
    eprintln!("brk-extend milestone seen after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must be present");
    let old_off = pre_pos + marker_pre.len();
    let old_bytes: [u8; 4] = cumulative[old_off..old_off + 4]
        .try_into()
        .expect("4 bytes for old brk");
    let old_brk = u32::from_le_bytes(old_bytes);

    let mid_pos = cumulative
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid must follow marker_pre");
    let new_off = mid_pos + marker_mid.len();
    let new_bytes: [u8; 4] = cumulative[new_off..new_off + 4]
        .try_into()
        .expect("4 bytes for new brk");
    let new_brk = u32::from_le_bytes(new_bytes);

    eprintln!(
        "old_brk = 0x{old_brk:08X}, new_brk = 0x{new_brk:08X}, delta = {} bytes",
        new_brk.wrapping_sub(old_brk),
    );

    assert_eq!(
        new_brk,
        old_brk.wrapping_add(0x1000),
        "expected new_brk = old_brk + 0x1000 (one page), but got old=0x{old_brk:08X} \
         new=0x{new_brk:08X}; either the kernel rejected the grow (returned old) or \
         allocated a different size; {}",
        dump_uart_on_failure(&cumulative, "brk-extend-mismatch")
    );
}

/// Argv passing milestone: /init forks; child execve's /helper
/// with argv `["/helper", "ARG1"]`; /helper reads `argv[1]` off
/// its stack and writes the pointed-to string between markers.
/// Test asserts the 4 bytes between `[USERSPACE ARGV1=` and
/// `][USERSPACE END]` equal `b"ARG1"`. Pins the kernel's
/// `copy_strings` path in execve (skipped when argv is NULL)
/// plus the i386 SysV process-startup ABI: at entry,
/// `[esp+0]=argc, [esp+4]=argv[0], [esp+8]=argv[1]`.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_argv_milestone() {
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
    let cpio = build_initramfs_argv();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE ARGV1=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let marker_failed: &[u8] = b"[USERSPACE EXECVE_FAILED]";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            let cause = if cumulative
                .windows(marker_failed.len())
                .any(|w| w == marker_failed)
            {
                "execve(\"/helper\", argv, NULL) returned to /init's child — the \
                 copy_strings/argv path may be rejecting the argv layout"
            } else {
                "no marker at all — fork failed before execve, or kernel hung in \
                 execve before reaching userspace"
            };
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {cause}; {}",
                dump_uart_on_failure(&cumulative, "argv")
            )
        });
    eprintln!("argv milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let argv1_bytes: [u8; 4] = cumulative[buf_start..buf_start + 4]
        .try_into()
        .expect("4 bytes between markers");
    eprintln!(
        "argv[1] returned: {:?} (raw: {:02X} {:02X} {:02X} {:02X})",
        std::str::from_utf8(&argv1_bytes).unwrap_or("<non-utf8>"),
        argv1_bytes[0],
        argv1_bytes[1],
        argv1_bytes[2],
        argv1_bytes[3],
    );
    assert_eq!(
        &argv1_bytes,
        b"ARG1",
        "expected 4 bytes argv[1] = b\"ARG1\", got {:02X?}; {}",
        argv1_bytes,
        dump_uart_on_failure(&cumulative, "argv-wrong")
    );
    assert!(
        !cumulative
            .windows(marker_failed.len())
            .any(|w| w == marker_failed),
        "saw EXECVE_FAILED too — somehow both branches ran; {}",
        dump_uart_on_failure(&cumulative, "argv-both")
    );
}

/// Envp passing milestone: /init forks; child execve's /helper
/// with argv `["/helper"]` and envp `["KEY=VAL"]`; /helper reads
/// `envp[0]` off its stack and writes the pointed-to 7 bytes
/// between `[USERSPACE ENV=…][USERSPACE END]`. Test asserts the
/// 7 bytes equal `b"KEY=VAL"`. Pins the envp half of
/// `copy_strings` that the argv milestone didn't cover, and the
/// stack layout AFTER the argv NULL terminator.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_envp_milestone() {
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
    let cpio = build_initramfs_envp();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE ENV=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let marker_failed: &[u8] = b"[USERSPACE EXECVE_FAILED]";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            let cause = if cumulative
                .windows(marker_failed.len())
                .any(|w| w == marker_failed)
            {
                "execve(\"/helper\", argv, envp) returned to /init's child — the \
                 envp copy_strings path may have rejected the envp layout"
            } else {
                "no marker — fork failed before execve, or kernel hung in execve \
                 before reaching userspace"
            };
            panic!(
                "`[USERSPACE END]` marker not seen in 16 B steps; {cause}; {}",
                dump_uart_on_failure(&cumulative, "envp")
            )
        });
    eprintln!("envp milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let env_bytes: [u8; 7] = cumulative[buf_start..buf_start + 7]
        .try_into()
        .expect("7 bytes between markers");
    eprintln!(
        "envp[0] returned: {:?}",
        std::str::from_utf8(&env_bytes).unwrap_or("<non-utf8>"),
    );
    assert_eq!(
        &env_bytes,
        b"KEY=VAL",
        "expected envp[0] = b\"KEY=VAL\", got {:02X?}; {}",
        env_bytes,
        dump_uart_on_failure(&cumulative, "envp-wrong")
    );
    assert!(
        !cumulative
            .windows(marker_failed.len())
            .any(|w| w == marker_failed),
        "saw EXECVE_FAILED too — somehow both branches ran; {}",
        dump_uart_on_failure(&cumulative, "envp-both")
    );
}

/// mmap2 milestone: /init asks the kernel for one anonymous
/// page via `sys_mmap2(NULL, 0x1000, R|W, ANON|PRIVATE, -1, 0)`,
/// writes 0x42 to the first byte, reads it back, and prints
/// `[USERSPACE MMAP=<addr>VAL=<byte>][USERSPACE END]`. Test
/// asserts addr is in userspace + page-aligned + the byte round-
/// trips. Different mechanism than brk: mmap allocates a
/// separate VMA at a kernel-chosen address; brk extends the
/// heap region contiguous with data segment.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_mmap_milestone() {
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
    let cpio = build_initramfs_mmap();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE MMAP=";
    let marker_mid: &[u8] = b"VAL=";
    let marker_end_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — mmap may have returned \
                 -errno and the subsequent write to a bad pointer SIGSEGV'd /init; {}",
                dump_uart_on_failure(&cumulative, "mmap")
            )
        });
    eprintln!("mmap milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_end");
    let addr_off = pre_pos + marker_pre.len();
    let addr_bytes: [u8; 4] = cumulative[addr_off..addr_off + 4]
        .try_into()
        .expect("4 bytes for mmap addr");
    let mmap_addr = u32::from_le_bytes(addr_bytes);

    let mid_pos = cumulative
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid must follow marker_pre");
    let val_byte = cumulative[mid_pos + marker_mid.len()];

    eprintln!("mmap returned addr = 0x{mmap_addr:08X}, byte read back = 0x{val_byte:02X}");

    // Same address bounds as the brk milestone: in userspace
    // virtual range, page-aligned.
    const KERNEL_BASE: u32 = 0xC000_0000;
    assert!(
        (INIT_LOAD_ADDR..KERNEL_BASE).contains(&mmap_addr),
        "mmap addr 0x{mmap_addr:08X} outside expected range \
         [0x{INIT_LOAD_ADDR:08X}, 0x{KERNEL_BASE:08X}); {}",
        dump_uart_on_failure(&cumulative, "mmap-range")
    );
    assert_eq!(
        mmap_addr & 0xFFF,
        0,
        "mmap addr 0x{mmap_addr:08X} not page-aligned (low 12 = {:#X}); {}",
        mmap_addr & 0xFFF,
        dump_uart_on_failure(&cumulative, "mmap-align")
    );
    assert_eq!(
        val_byte,
        0x42,
        "expected byte read back to equal sentinel 0x42, got 0x{val_byte:02X} — \
         either the page isn't backed by RAM or writes aren't persisting; {}",
        dump_uart_on_failure(&cumulative, "mmap-byte")
    );
}

/// File-I/O round-trip milestone: /init creates a new file in
/// initramfs (`/test_file`) with `open(O_CREAT|O_WRONLY, 0o644)`,
/// writes `b"TESTDATA"` to it, closes, reopens read-only, reads
/// the 8 bytes back, prints them between
/// `[USERSPACE FILE=…][USERSPACE END]`. Test asserts the round-
/// tripped bytes equal `b"TESTDATA"`. Pins:
///   - writable initramfs (tmpfs-backed rootfs)
///   - `do_filp_open` with O_CREAT — every prior open() in tests
///     used existing files (/proc/version, /helper)
///   - `sys_close` — never previously exercised
///   - cross-fd persistence via tmpfs page cache
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_file_io_milestone() {
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
    let cpio = build_initramfs_file_io();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE FILE=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — open(O_CREAT) may have \
                 returned -EROFS / -EPERM, or write/read failed silently; {}",
                dump_uart_on_failure(&cumulative, "file-io")
            )
        });
    eprintln!("file_io milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let file_bytes: [u8; 8] = cumulative[buf_start..buf_start + 8]
        .try_into()
        .expect("8 bytes between markers");
    eprintln!(
        "file content round-tripped: {:?}",
        std::str::from_utf8(&file_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        &file_bytes,
        b"TESTDATA",
        "expected file round-trip to equal b\"TESTDATA\", got {:02X?}; {}",
        file_bytes,
        dump_uart_on_failure(&cumulative, "file-io-wrong")
    );
}

/// File metadata milestone: /init creates `/probe` with 8 bytes
/// of "TESTDATA", then calls `sys_stat64("/probe", &statbuf)`
/// (syscall 195) and reads `st_size` (offset 44, low 4 bytes)
/// back out. Writes the size between
/// `[USERSPACE STAT_SIZE=<4 bytes>][USERSPACE END]`. Test asserts
/// the decoded value equals 8.
///
/// Pins the inode metadata path independently of read/write:
/// stat fills a ~96-byte struct via `cp_new_stat64` →
/// `copy_to_user`. Different code path than fd-based read.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_stat_milestone() {
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
    let cpio = build_initramfs_stat();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE STAT_SIZE=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — stat64 may have returned \
                 -errno or the struct layout offset is wrong; {}",
                dump_uart_on_failure(&cumulative, "stat")
            )
        });
    eprintln!("stat milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let size_off = pre_pos + marker_pre.len();
    let size_bytes: [u8; 4] = cumulative[size_off..size_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let st_size = u32::from_le_bytes(size_bytes);
    eprintln!("stat returned st_size = {st_size}");
    assert_eq!(
        st_size,
        8,
        "expected st_size = 8 (we wrote 'TESTDATA' = 8 bytes), got {st_size}; \
         could be stat64 failed (st_size = 0 from initial zeros) or struct offset \
         is wrong; {}",
        dump_uart_on_failure(&cumulative, "stat-wrong")
    );
}

/// Random-access I/O milestone: /init creates `/probe` with 8
/// bytes "TESTDATA", calls `sys_lseek(fd, 4, SEEK_SET)`, reads 4
/// bytes — should be `b"DATA"` (offsets 4..8 of the original).
/// Writes them between `[USERSPACE LSEEK=…][USERSPACE END]`.
/// Test asserts the bytes equal "DATA". Pins the kernel's
/// `struct file` position bookkeeping; sequential read at
/// offset 0 (the file-io milestone) doesn't move it.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_lseek_milestone() {
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
    let cpio = build_initramfs_lseek();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE LSEEK=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — lseek may have returned \
                 -errno or the subsequent read couldn't reach the offset; {}",
                dump_uart_on_failure(&cumulative, "lseek")
            )
        });
    eprintln!("lseek milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let lseek_bytes: [u8; 4] = cumulative[buf_start..buf_start + 4]
        .try_into()
        .expect("4 bytes between markers");
    eprintln!(
        "lseek+read returned: {:?}",
        std::str::from_utf8(&lseek_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        &lseek_bytes,
        b"DATA",
        "expected 4 bytes after lseek(4) to equal b\"DATA\" (offsets 4..8 of \
         'TESTDATA'), got {:02X?} — if it equals b\"TEST\" the seek didn't \
         take effect; {}",
        lseek_bytes,
        dump_uart_on_failure(&cumulative, "lseek-wrong")
    );
}

/// `sys_dup2` milestone: /init opens `/log` for write, dup2's
/// its fd onto fd 1, writes "REDIRECTED" via fd 1 (should land
/// in /log, NOT UART), closes the fds, reopens /log, reads it
/// back, prints via fd 2 (stderr → /dev/console → UART). Test
/// asserts the 10 bytes between markers equal `b"REDIRECTED"`.
///
/// Foundation for shell I/O redirection: `cmd > file` is exactly
/// `open(file) + dup2(fd_file, 1) + close(fd_file) + exec(cmd)`.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_dup2_milestone() {
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
    let cpio = build_initramfs_dup2();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE DUP2=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — dup2 may have failed, \
                 or write to fd 2 doesn't reach UART; {}",
                dump_uart_on_failure(&cumulative, "dup2")
            )
        });
    eprintln!("dup2 milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let dup2_bytes: [u8; 10] = cumulative[buf_start..buf_start + 10]
        .try_into()
        .expect("10 bytes between markers");
    eprintln!(
        "dup2 round-trip via /log: {:?}",
        std::str::from_utf8(&dup2_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        &dup2_bytes,
        b"REDIRECTED",
        "expected 10 bytes round-tripped via /log to equal b\"REDIRECTED\", got \
         {:02X?} — if all zeros, dup2 silently failed and /log was empty when \
         we tried to read it back; {}",
        dup2_bytes,
        dump_uart_on_failure(&cumulative, "dup2-wrong")
    );
}

/// File-deletion milestone: /init creates `/probe`, calls
/// `sys_unlink("/probe")`, then `sys_stat64("/probe", …)` which
/// MUST fail with `-ENOENT`. /init writes the stat-after-unlink
/// return code between `[USERSPACE UNLINKED_STAT=…][USERSPACE END]`.
/// Test asserts the value equals `0xFFFFFFFE` (`-2` sign-extended).
///
/// Pins the kernel's unlink path AND the negative-result return
/// ABI: every previous milestone's syscalls succeeded, so an
/// implicit assumption was that the kernel always returns 0 on
/// success. This test's whole point is that the kernel correctly
/// signals failure via `-errno` in eax.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_unlink_milestone() {
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
    let cpio = build_initramfs_unlink();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE UNLINKED_STAT=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — unlink may have hung, \
                 or stat after unlink crashed; {}",
                dump_uart_on_failure(&cumulative, "unlink")
            )
        });
    eprintln!("unlink milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let off = pre_pos + marker_pre.len();
    let ret_bytes: [u8; 4] = cumulative[off..off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let ret = u32::from_le_bytes(ret_bytes);
    eprintln!(
        "stat after unlink returned 0x{ret:08X} = {} (signed)",
        ret as i32
    );
    // -ENOENT = -2 ⇒ 0xFFFFFFFE as u32 LE.
    assert_eq!(
        ret,
        0xFFFF_FFFE,
        "expected stat-after-unlink to return -ENOENT (-2 = 0xFFFFFFFE), got \
         0x{ret:08X} — if 0, unlink didn't actually delete the file (stat still \
         finds it); {}",
        dump_uart_on_failure(&cumulative, "unlink-wrong")
    );
}

/// Directory + nested file milestone: /init creates `/dir` via
/// `sys_mkdir`, then creates `/dir/test` with 5 bytes "INDIR",
/// closes, reopens, reads back, prints. Test asserts the
/// 5 bytes round-trip equal `b"INDIR"`. Pins kernel directory
/// inode allocation AND multi-level path resolution (every
/// previous milestone used flat paths at /).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_mkdir_milestone() {
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
    let cpio = build_initramfs_mkdir();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE FILE_IN_DIR=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — mkdir may have returned \
                 -errno or the subsequent open failed; {}",
                dump_uart_on_failure(&cumulative, "mkdir")
            )
        });
    eprintln!("mkdir milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let file_bytes: [u8; 5] = cumulative[buf_start..buf_start + 5]
        .try_into()
        .expect("5 bytes between markers");
    eprintln!(
        "file in /dir round-tripped: {:?}",
        std::str::from_utf8(&file_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        &file_bytes,
        b"INDIR",
        "expected round-trip = b\"INDIR\", got {:02X?} — if all zeros, either \
         mkdir didn't create /dir (subsequent open failed with -ENOENT) or the \
         write into /dir/test never happened; {}",
        file_bytes,
        dump_uart_on_failure(&cumulative, "mkdir-wrong")
    );
}

/// Vectored I/O milestone: /init calls `sys_writev` (syscall 146)
/// with two iovec entries that together spell
/// `[USERSPACE WRITEV=AB][USERSPACE END]\n`. Test asserts the
/// concatenated string appears in UART. Pins the kernel's
/// iovec-walking write path: until now every write milestone
/// used a single contiguous buffer.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_writev_milestone() {
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
    let cpio = build_initramfs_writev();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let needle: &[u8] = b"[USERSPACE WRITEV=AB][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps =
        run_until_marker(&mut vm, needle, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
            panic!(
                "concatenated writev output not seen in 16 B steps — sys_writev may \
                 have returned -errno or split the iovecs differently; {}",
                dump_uart_on_failure(&cumulative, "writev")
            )
        });
    eprintln!("writev milestone full string seen after {steps} steps");
}

/// Timer + scheduler milestone: /init samples `sys_time` before
/// and after `sys_nanosleep(1 sec)`, writes both time samples
/// between markers. Test asserts `t1 - t0 >= 1` — proves the
/// kernel actually held the process off for 1 second of guest
/// wall time. Until now every milestone ran in "instant" guest
/// time; this pins PIT/LAPIC IRQs → jiffies → timer wheel →
/// task wakeup end-to-end.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_nanosleep_milestone() {
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
    let cpio = build_initramfs_nanosleep();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE T0=";
    let marker_mid: &[u8] = b"T1=";
    let marker_end_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — nanosleep may have \
                 returned -errno or never woken up; {}",
                dump_uart_on_failure(&cumulative, "nanosleep")
            )
        });
    eprintln!("nanosleep milestone end marker after {steps} steps");

    // The kernel TTY ONLCR-translates every `\n` /init writes
    // into `\r\n` — fine for text markers, but for binary t0/t1
    // bytes it pads the stream and shifts decoded u32 values.
    // Undo the translation: any `\r` followed by `\n` was added
    // by the kernel, not by /init. Strip those `\r`s into a
    // separate buffer used for binary extraction; keep the
    // original cumulative for marker searches that already
    // include the `\r\n`.
    let stripped: Vec<u8> = cumulative
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| {
            if b == b'\r' && cumulative.get(i + 1) == Some(&b'\n') {
                None
            } else {
                Some(b)
            }
        })
        .collect();

    let pre_pos = stripped
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre");
    let t0_off = pre_pos + marker_pre.len();
    let t0 = u32::from_le_bytes(stripped[t0_off..t0_off + 4].try_into().unwrap());

    let mid_pos = stripped
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid");
    let t1_off = mid_pos + marker_mid.len();
    let t1 = u32::from_le_bytes(stripped[t1_off..t1_off + 4].try_into().unwrap());

    eprintln!(
        "t0 = {t0}, t1 = {t1}, delta = {} seconds",
        t1.wrapping_sub(t0)
    );
    assert!(
        t1 > t0,
        "expected t1 > t0 (kernel slept at least 1 second), got t0={t0} t1={t1} \
         (delta = {}); nanosleep may not have actually waited; {}",
        t1.wrapping_sub(t0),
        dump_uart_on_failure(&cumulative, "nanosleep-no-wait")
    );
}

/// Diagnostic test for the `chdir`+`getcwd` blocker discovered
/// 2026-05-29: /init makes `/sub`, chdir's into it, then asks
/// for the current directory via `sys_getcwd(buf, 32)`. Doesn't
/// assert — logs `buf` (the path the kernel wrote) AND
/// `getcwd`'s eax return. First green run showed:
///
///     buf = [0, 0, 0, 0] (all zeros, no path written)
///     ret = 0x00000000 (zero — neither success len ≥ 2 nor -errno)
///
/// Linux's `sys_getcwd` (fs/d_path.c) either returns a positive
/// length on success (≥ 2 for "/" + NUL) or a negative errno —
/// 0 isn't in the contract. So either the syscall isn't running
/// at all (maybe syscall 183 isn't getcwd in our kernel build?)
/// or it's running but eax is being clobbered before we capture
/// it. Next investigation tick should compare with another
/// kernel build OR sample the eax at a finer granularity.
#[test]
#[ignore = "diagnostic for chdir/getcwd; sys_getcwd returns 0 instead of path length"]
fn linux_userspace_chdir_diag() {
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
    let cpio = build_initramfs_chdir();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE CWD=";
    let marker_ret: &[u8] = b" RET=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — chdir or getcwd may have \
                 failed; {}",
                dump_uart_on_failure(&cumulative, "chdir")
            )
        });
    eprintln!("chdir milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let cwd_bytes: [u8; 4] = cumulative[buf_start..buf_start + 4]
        .try_into()
        .expect("4 bytes between markers");
    let ret_pos = cumulative
        .windows(marker_ret.len())
        .position(|w| w == marker_ret)
        .expect("marker_ret");
    let ret_off = ret_pos + marker_ret.len();
    let ret_bytes: [u8; 4] = cumulative[ret_off..ret_off + 4]
        .try_into()
        .expect("4 bytes for getcwd return");
    let getcwd_ret = u32::from_le_bytes(ret_bytes);
    eprintln!("=== chdir + getcwd diagnostic ===");
    eprintln!(
        "  cwd_bytes (first 4 of buf) = {:02X?} = {:?}",
        cwd_bytes,
        std::str::from_utf8(&cwd_bytes).unwrap_or("<non-utf8>")
    );
    eprintln!(
        "  getcwd ret = 0x{getcwd_ret:08X} ({} signed)",
        getcwd_ret as i32
    );
    if cwd_bytes == *b"/sub" {
        eprintln!("  → chdir + getcwd WORKING");
    } else if getcwd_ret == 0 {
        eprintln!(
            "  → getcwd returned 0 (not in Linux contract) — kernel-side investigation needed"
        );
    } else if (getcwd_ret as i32) < 0 {
        eprintln!(
            "  → getcwd failed with errno {} — check Linux i386 errno list",
            -(getcwd_ret as i32)
        );
    } else {
        eprintln!(
            "  → getcwd returned positive {} but buf doesn't match expected '/sub'",
            getcwd_ret
        );
    }
}

/// Diagnostic test for the `sys_pipe`-broken blocker: runs the
/// minimal `build_initramfs_pipe_diag` /init and prints whatever
/// sys_pipe2 returned plus the two fd slots. ALWAYS PASSES — its
/// purpose is to surface the exact return code in the test log
/// so the next investigation tick has data to act on. If the
/// return is 0 (success), we know sys_pipe2 works and the
/// earlier round-trip test had a different bug.
#[test]
#[ignore = "diagnostic for sys_pipe blocker; logs return code, ~52s wall-clock"]
fn linux_userspace_pipe_diag() {
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
    let cpio = build_initramfs_pipe_diag();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE PIPE_RET=";
    let marker_fd0: &[u8] = b" FD0=";
    let marker_fd1: &[u8] = b" FD1=";
    let marker_end_search: &[u8] = b" END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    if run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative).is_err() {
        eprintln!(
            "diagnostic timed out — pipe diag never reached end marker. dump: {}",
            dump_uart_on_failure(&cumulative, "pipe-diag")
        );
        return;
    }

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre");
    let ret_off = pre_pos + marker_pre.len();
    let ret = u32::from_le_bytes(cumulative[ret_off..ret_off + 4].try_into().unwrap());

    let fd0_pos = cumulative
        .windows(marker_fd0.len())
        .position(|w| w == marker_fd0)
        .expect("marker_fd0");
    let fd0_off = fd0_pos + marker_fd0.len();
    let fd0 = u32::from_le_bytes(cumulative[fd0_off..fd0_off + 4].try_into().unwrap());

    let fd1_pos = cumulative
        .windows(marker_fd1.len())
        .position(|w| w == marker_fd1)
        .expect("marker_fd1");
    let fd1_off = fd1_pos + marker_fd1.len();
    let fd1 = u32::from_le_bytes(cumulative[fd1_off..fd1_off + 4].try_into().unwrap());

    eprintln!("=== sys_pipe2 diagnostic ===");
    eprintln!("  ret = 0x{ret:08X} (signed: {})", ret as i32);
    eprintln!("  fds[0] = {fd0}");
    eprintln!("  fds[1] = {fd1}");
    if ret == 0 {
        eprintln!("  → sys_pipe2 SUCCEEDED — the earlier round-trip test had a different bug");
    } else {
        eprintln!(
            "  → sys_pipe2 FAILED with errno {} — see Linux i386 errno list",
            -(ret as i32)
        );
    }
}
