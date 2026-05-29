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
//!   - `linux_userspace_rename_milestone` — file rename: /init
//!     creates `/a` with "RENDATA", calls `sys_rename("/a",
//!     "/b")`, opens `/b`, reads content back, prints. Test
//!     asserts the round-trip equals `b"RENDATA"`. Pins the
//!     kernel's dentry rename path — the inode moves with the
//!     dentry, the content stays intact, and the OLD path
//!     becomes inaccessible (the read goes through /b's new
//!     dentry).
//!
//!   - `linux_userspace_truncate_milestone` — file shrink:
//!     /init writes a 10-byte file, calls `sys_truncate(path,
//!     4)`, then `sys_stat64` and reads `st_size` back. Test
//!     asserts the value equals 4 — proves the kernel
//!     SHRANK the file. The stat milestone only pins write
//!     extending i_size; this one pins truncate shrinking it.
//!
//!   - `linux_userspace_chmod_milestone` — mode-bit
//!     manipulation: /init creates `/probe` with mode 0o644,
//!     calls `sys_chmod(path, 0o600)`, stats it, reads
//!     `st_mode`. Test asserts the value equals
//!     `S_IFREG | 0o600 = 0o100600`. Pins the kernel's
//!     `setattr → notify_change` path that updates
//!     `inode->i_mode` AND stat reflecting the change.
//!
//!   - `linux_userspace_getppid_in_child_milestone` —
//!     parent-child link: /init forks; child calls
//!     `sys_getppid` which returns /init's PID (= 1); test
//!     asserts equality. Pins the kernel's `real_parent`
//!     setup during fork — if the kernel left child's
//!     `real_parent` pointing at swapper or kthreadd, the
//!     test would see 0 or 2 instead of 1.
//!
//!   - `linux_userspace_access_milestone` — path existence
//!     check, both success and failure: /init creates
//!     `/probe`, calls `sys_access(F_OK)` (expect 0), unlinks,
//!     calls again (expect -ENOENT = 0xFFFFFFFE). Test asserts
//!     the pair. First test that pairs the success and
//!     failure-on-purpose return paths in one shot.
//!
//!   - `linux_userspace_statfs_milestone` — filesystem
//!     metadata: /init calls `sys_statfs64("/", 84, &buf)`
//!     (syscall 268), reads `f_type`. Test asserts the value
//!     equals `TMPFS_MAGIC = 0x01021994` — proves rootfs is
//!     tmpfs AND the kernel filled the statfs struct. NOTE:
//!     legacy `sys_statfs` (syscall 99) returned 0/error in
//!     our kernel build; modern Linux prefers statfs64.
//!
//!   - `linux_userspace_fcntl_milestone` — fd-flag storage:
//!     /init opens a file, sets FD_CLOEXEC via
//!     `sys_fcntl(fd, F_SETFD, 1)`, then reads it back via
//!     `sys_fcntl(fd, F_GETFD, 0)`. Test asserts the returned
//!     flags equal 1. Pins the fd-flag storage in the file-
//!     descriptor table entry — distinct from open-file flags
//!     (O_CREAT etc.) that live in the file struct itself.
//!
//!   - `linux_userspace_sysinfo_milestone` — system info: /init
//!     calls `sys_sysinfo(&buf)`, reads `uptime` (first field of
//!     the 64-byte struct). Test asserts the value is positive.
//!     Pins the kernel's sysinfo path that fills uptime + load
//!     averages + memory totals + procs count via copy_to_user.
//!
//!   - `linux_userspace_mprotect_milestone` — page protection
//!     change: /init mmap's R+W, writes 0x42, mprotects to R
//!     only, reads back. Test asserts ret == 0 AND byte == 0x42.
//!     Pins kernel's per-VMA `vm_flags` mutation by mprotect AND
//!     data preservation across permission change. Doesn't test
//!     write-after-mprotect (would SIGSEGV /init).
//!
//!   - `linux_userspace_signal_milestone` — one-way signal
//!     delivery: /init installs a SIGUSR1 handler via
//!     `sys_rt_sigaction`, sends SIGUSR1 to self via `sys_kill`.
//!     Handler writes `[USERSPACE HANDLER]` then `sys_exit(0)`
//!     directly (skipping sigreturn, which is broken in our
//!     emulation — verified empirically: handler returning via
//!     `ret` → SIGSEGV in /init, exitcode=11). Test verifies the
//!     marker appears. Pins sigaction storage + kill queuing +
//!     kernel jumping to the handler.
//!
//!   - `linux_userspace_symlink_milestone` — symlink create +
//!     read: /init `sys_symlink("/target", "/link")` then
//!     `sys_readlink("/link", buf, 32)`. Test asserts
//!     `sym_ret == 0`, `rl_ret == 7`, and the readlink buffer
//!     equals `b"/target"`. Pins symlink inode creation
//!     (S_IFLNK in tmpfs) AND readlink returning the link body
//!     string + byte count (without following the link).
//!
//!   - `linux_userspace_uname_milestone` — `sys_uname` (122):
//!     /init fills `struct new_utsname`, writes `sysname[0..65]`;
//!     test asserts it is "Linux". Built via the safe wrapper
//!     (`build_initramfs_uname_safe`), so it also validates that
//!     `make_init_elf32_safe` neutralizes the historical 600-byte
//!     hang that the raw `build_initramfs_uname` still reproduces.
//!
//!   - `linux_userspace_hardlink_milestone` — `sys_link` (9):
//!     /init writes `/a`, hard-links it to `/b`, reads `/b` back.
//!     Test asserts link_ret == 0 and content == `b"HARDLINK"`.
//!     Distinct from symlink: a hard link is a second dentry to
//!     the SAME inode (shared data blocks), not a separate inode.
//!
//!   - `linux_userspace_getdents_milestone` — `sys_getdents64`
//!     (220): /init mkdir's `/d`, creates `/d/ZZMARKER`, opens
//!     `/d` and enumerates it. Test asserts the returned byte
//!     count > 0 and the dirent buffer contains `b"ZZMARKER"`.
//!     Pins directory enumeration — the `ls`/`readdir` primitive.
//!
//!   - `linux_userspace_chdir_milestone` — `sys_chdir` (12) +
//!     `sys_getcwd` (183): /init mkdir's `/sub`, chdir's into it,
//!     reads cwd back. Test asserts mkdir==0, chdir==0, getcwd==5,
//!     cwd=="/sub". (Formerly a diagnostic for a "getcwd returns
//!     0" blocker that turned out to be a test-harness bug —
//!     getcwd works fine. See the test for the post-mortem.)
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

/// Build a cpio with /init plus shared libraries under /lib — the
/// initramfs shape a *dynamically-linked* /init needs. The kernel
/// execs /init, sees its `PT_INTERP` (`/lib/ld-linux.so.2`), loads
/// that interpreter, and the interpreter then opens + mmaps the
/// `lib_files` to satisfy the binary's `DT_NEEDED` libraries.
/// `lib_files` are `(basename, bytes)` placed at `/lib/<basename>`.
fn build_cpio_with_libs(init_binary: &[u8], lib_files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio_entry("proc", &[], 0o040_755, 0, 0));
    if !lib_files.is_empty() {
        archive.extend_from_slice(&cpio_entry("lib", &[], 0o040_755, 0, 0));
        for (name, bytes) in lib_files {
            let path = format!("lib/{name}");
            archive.extend_from_slice(&cpio_entry(&path, bytes, 0o100_755, 0, 0));
        }
    }
    archive.extend_from_slice(&cpio_entry("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    archive
}

/// Build a cpio with /init plus an arbitrary set of regular files at
/// given paths (e.g. `bin/busybox`, `lib/libc.so.6`), auto-creating
/// each file's single-level parent directory. Used to assemble a
/// minimal rootfs for running a real dynamically-linked program: the
/// /init stub `execve`s `/bin/busybox`, whose interpreter loads the
/// libraries from `/lib`.
fn build_cpio_tree(init_binary: &[u8], files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio_entry("init", init_binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio_entry("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio_entry("proc", &[], 0o040_755, 0, 0));
    // Create each unique single-level parent dir before its files.
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

/// Hand-assembled /init that `execve("/bin/busybox", ["busybox",
/// "echo", "DYNLINK_OK"], [])`s — a tiny static syscall stub whose
/// only job is to hand off to a real dynamically-linked program so
/// the kernel + ld.so run it. On execve failure it prints
/// `[EXECVE-FAIL]` and exits 1.
fn build_init_execve_busybox() -> Vec<u8> {
    let path: &[u8] = b"/bin/busybox\0";
    let s_busybox: &[u8] = b"busybox\0";
    let s_echo: &[u8] = b"echo\0";
    let s_dyn: &[u8] = b"DYNLINK_OK\0";
    let failmsg: &[u8] = b"[EXECVE-FAIL]\n";

    let build_code =
        |path_addr: u32, argv_addr: u32, envp_addr: u32, failmsg_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(64);
            // execve(path, argv, envp) — sys 11
            out.extend_from_slice(&[0xB8, 0x0B, 0x00, 0x00, 0x00]); // mov eax, 11
            out.push(0xBB);
            out.extend_from_slice(&path_addr.to_le_bytes());
            out.push(0xB9);
            out.extend_from_slice(&argv_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&envp_addr.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // Only reached if execve failed: write [EXECVE-FAIL], exit(1).
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
    let s_busybox_addr = path_addr + path.len() as u32;
    let s_echo_addr = s_busybox_addr + s_busybox.len() as u32;
    let s_dyn_addr = s_echo_addr + s_echo.len() as u32;
    let failmsg_addr = s_dyn_addr + s_dyn.len() as u32;
    let argv_addr = failmsg_addr + failmsg.len() as u32;
    let envp_addr = argv_addr + 16; // argv = 4 u32 (3 ptrs + NULL)
    let code = build_code(path_addr, argv_addr, envp_addr, failmsg_addr);
    // argv = [&"busybox", &"echo", &"DYNLINK_OK", NULL]; envp = [NULL].
    let mut argv = Vec::with_capacity(16);
    argv.extend_from_slice(&s_busybox_addr.to_le_bytes());
    argv.extend_from_slice(&s_echo_addr.to_le_bytes());
    argv.extend_from_slice(&s_dyn_addr.to_le_bytes());
    argv.extend_from_slice(&0u32.to_le_bytes());
    let envp = 0u32.to_le_bytes();
    make_init_elf32_safe(
        &code,
        &[path, s_busybox, s_echo, s_dyn, failmsg, &argv, &envp],
        7,
    )
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

/// Production-safe variant of the uname /init: calls `sys_uname`
/// (syscall 122), filling a `struct new_utsname` (6 × 65-byte
/// fields: sysname, nodename, release, version, machine,
/// domainname), then writes `sysname[0..65]` between
/// `[USERSPACE UNAME=…][USERSPACE END]` markers. Unlike
/// `build_initramfs_uname` — which deliberately uses raw
/// `make_init_elf32` to land at exactly 600 bytes and reproduce
/// the historical {600,602}-bad-size boot stall — this one
/// routes through `make_init_elf32_safe`, so if it lands on a
/// known-bad size it gets a 4-byte tail-pad to dodge it. This
/// milestone therefore doubles as live validation that the
/// safety wrapper actually neutralizes the original hang that
/// started the whole bisection saga.
fn build_initramfs_uname_safe() -> Vec<u8> {
    const UTSNAME_LEN: u32 = 390;
    let marker_pre: &[u8] = b"[USERSPACE UNAME=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_uname(&buf) — sys 122
        out.extend_from_slice(&[0xB8, 0x7A, 0x00, 0x00, 0x00]); // mov eax, 122
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, buf (sysname), 65)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x41, 0x00, 0x00, 0x00]); // mov edx, 65
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

    let code_len = build_code(0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr);
    let buf_zeros = vec![0u8; UTSNAME_LEN as usize];
    let binary = make_init_elf32_safe(&code, &[marker_pre, marker_post, &buf_zeros], 7);
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
    let rename = build_initramfs_rename();
    let truncate = build_initramfs_truncate();
    let chmod = build_initramfs_chmod();
    let getppid_in_child = build_initramfs_getppid_in_child();
    let access = build_initramfs_access();
    let statfs = build_initramfs_statfs();
    let fcntl = build_initramfs_fcntl();
    let sysinfo = build_initramfs_sysinfo();
    let mprotect = build_initramfs_mprotect();
    let signal = build_initramfs_signal();
    let symlink = build_initramfs_symlink();
    let uname_safe = build_initramfs_uname_safe();
    let hardlink = build_initramfs_hardlink();
    let getdents = build_initramfs_getdents();
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
    assert_eq!(&rename[0..6], b"070701");
    assert_eq!(&truncate[0..6], b"070701");
    assert_eq!(&chmod[0..6], b"070701");
    assert_eq!(&getppid_in_child[0..6], b"070701");
    assert_eq!(&access[0..6], b"070701");
    assert_eq!(&statfs[0..6], b"070701");
    assert_eq!(&fcntl[0..6], b"070701");
    assert_eq!(&sysinfo[0..6], b"070701");
    assert_eq!(&mprotect[0..6], b"070701");
    assert_eq!(&signal[0..6], b"070701");
    assert_eq!(&symlink[0..6], b"070701");
    assert_eq!(&uname_safe[0..6], b"070701");
    assert_eq!(&hardlink[0..6], b"070701");
    assert_eq!(&getdents[0..6], b"070701");
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
        let _ = std::fs::write("/tmp/wwwvm-rename.cpio", &rename);
        let _ = std::fs::write("/tmp/wwwvm-truncate.cpio", &truncate);
        let _ = std::fs::write("/tmp/wwwvm-chmod.cpio", &chmod);
        let _ = std::fs::write("/tmp/wwwvm-getppid-in-child.cpio", &getppid_in_child);
        let _ = std::fs::write("/tmp/wwwvm-access.cpio", &access);
        let _ = std::fs::write("/tmp/wwwvm-statfs.cpio", &statfs);
        let _ = std::fs::write("/tmp/wwwvm-fcntl.cpio", &fcntl);
        let _ = std::fs::write("/tmp/wwwvm-sysinfo.cpio", &sysinfo);
        let _ = std::fs::write("/tmp/wwwvm-mprotect.cpio", &mprotect);
        let _ = std::fs::write("/tmp/wwwvm-signal.cpio", &signal);
        let _ = std::fs::write("/tmp/wwwvm-symlink.cpio", &symlink);
        let _ = std::fs::write("/tmp/wwwvm-uname-safe.cpio", &uname_safe);
        let _ = std::fs::write("/tmp/wwwvm-hardlink.cpio", &hardlink);
        let _ = std::fs::write("/tmp/wwwvm-getdents.cpio", &getdents);
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

/// Build a cpio whose /init creates a file, writes 8 bytes to it,
/// then maps it with `mmap2(NULL, 0x1000, PROT_READ, MAP_PRIVATE,
/// fd, 0)` — a *file-backed* mapping — and dumps the mapped bytes
/// to the console by `write(1, mapped_addr, 8)`:
///
///   `[USERSPACE FMAP=<4 addr>DATA=<8 bytes>][USERSPACE END]`
///
/// This is a distinct kernel path from the anonymous-mmap and
/// brk/COW milestones: the first access to the mapping (the
/// `write` syscall's `copy_from_user` reading `mapped_addr`)
/// triggers a *file* page fault — `filemap_fault`/`shmem_fault`
/// pulls the page from the file's page cache and installs it in
/// the process's address space. This is exactly how `ld.so` maps
/// shared-object text/data pages, so it's directly on the path to
/// a dynamically-linked userspace.
///
/// Assertions: the returned address is page-aligned and in
/// `[INIT_LOAD_ADDR, 0xC0000000)`, and the bytes read back through
/// the mapping equal the file's contents (`"MAPDATA8"`).
fn build_initramfs_file_mmap() -> Vec<u8> {
    let path: &[u8] = b"/mapped\0";
    let payload: &[u8] = b"MAPDATA8";
    let marker_pre: &[u8] = b"[USERSPACE FMAP=";
    let marker_mid: &[u8] = b"DATA=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      fd_addr: u32,
                      addr_buf: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // fd = open(path, O_CREAT|O_RDWR = 0x42, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x42, 0x00, 0x00, 0x00]); // mov ecx, 0x42
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[fd], eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // write(fd, payload, 8)
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // addr = mmap2(NULL, 0x1000, PROT_READ=1, MAP_PRIVATE=2, fd, 0)
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]); // mov eax, 192
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (addr=0)
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]); // mov ecx, 0x1000
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, PROT_READ
        out.extend_from_slice(&[0xBE, 0x02, 0x00, 0x00, 0x00]); // mov esi, MAP_PRIVATE
        out.extend_from_slice(&[0x8B, 0x3D]); // mov edi, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xED]); // xor ebp, ebp (pgoff=0)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[addr_buf], eax
        out.extend_from_slice(&addr_buf.to_le_bytes());
        // close(fd) — the mapping keeps its own reference to the file.
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // emit FMAP=<addr>
        w(marker_pre_addr, marker_pre.len() as u32, &mut out);
        w(addr_buf, 4, &mut out);
        // emit DATA=<mapped bytes>: write(1, [addr_buf], 8). Reading
        // through the mapping here is what faults the file page in.
        w(marker_mid_addr, marker_mid.len() as u32, &mut out);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.extend_from_slice(&[0x8B, 0x0D]); // mov ecx, ds:[addr_buf] (the mapped addr)
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        // emit END
        w(marker_end_addr, marker_end.len() as u32, &mut out);
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
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_end_addr = marker_mid_addr + marker_mid.len() as u32;
    let fd_addr = marker_end_addr + marker_end.len() as u32;
    let addr_buf = fd_addr + 4;
    let code = build_code(
        path_addr,
        payload_addr,
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        fd_addr,
        addr_buf,
    );
    let fd_zeros = [0u8; 4];
    let addr_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            payload,
            marker_pre,
            marker_mid,
            marker_end,
            &fd_zeros,
            &addr_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init writes a tiny machine-code function to a
/// file, maps it `PROT_READ|PROT_EXEC` (MAP_PRIVATE), and `CALL`s
/// into the mapping — proving the emulator can *fetch and execute*
/// instructions from a demand-faulted file-backed page.
///
/// The mapped "function" is the 3 bytes `CD 80 C3` (`int 0x80; ret`)
/// — a bare syscall trampoline. /init sets the syscall registers
/// (eax=write, ebx=1, ecx=marker, edx=len) and `CALL`s the mapping;
/// the mapped code runs the syscall (writing `[USERSPACE XMAP]`) and
/// `ret`s back. Keeping the data in /init's own pages sidesteps
/// i386's lack of RIP-relative addressing — the mapped page only has
/// to be *fetched and run*, which is the point.
///
/// Output: `[USERSPACE FMAP=<4 addr>][USERSPACE XMAP]\n[USERSPACE END]`.
/// The first call's instruction fetch faults the file page in via
/// the exec path — exactly how `ld.so` runs shared-object text. The
/// test asserts a valid page-aligned address and that the XMAP
/// marker (emitted *by the mapped code*) appears.
fn build_initramfs_exec_mmap() -> Vec<u8> {
    let path: &[u8] = b"/code\0";
    let codepayload: &[u8] = &[0xCD, 0x80, 0xC3]; // int 0x80 ; ret
    let marker_fmap: &[u8] = b"[USERSPACE FMAP=";
    let marker_x: &[u8] = b"[USERSPACE XMAP]\n";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      code_addr: u32,
                      marker_fmap_addr: u32,
                      marker_x_addr: u32,
                      marker_end_addr: u32,
                      fd_addr: u32,
                      addr_buf: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // fd = open(path, O_CREAT|O_RDWR = 0x42, 0o755)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x42, 0x00, 0x00, 0x00]); // O_CREAT|O_RDWR
        out.extend_from_slice(&[0xBA, 0xED, 0x01, 0x00, 0x00]); // 0o755
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[fd], eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // write(fd, codepayload, 3)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&code_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(codepayload.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // addr = mmap2(NULL, 0x1000, PROT_READ|PROT_EXEC=5, MAP_PRIVATE=2, fd, 0)
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0x05, 0x00, 0x00, 0x00]); // PROT_READ|PROT_EXEC
        out.extend_from_slice(&[0xBE, 0x02, 0x00, 0x00, 0x00]); // MAP_PRIVATE
        out.extend_from_slice(&[0x8B, 0x3D]); // mov edi, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xED]); // pgoff=0
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[addr_buf], eax
        out.extend_from_slice(&addr_buf.to_le_bytes());
        // close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // emit FMAP=<addr> before the call (so a bad addr is still
        // visible if the call faults).
        w(marker_fmap_addr, marker_fmap.len() as u32, &mut out);
        w(addr_buf, 4, &mut out);
        // Set up write(1, marker_x, len), then CALL the mapped code.
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9); // mov ecx, marker_x
        out.extend_from_slice(&marker_x_addr.to_le_bytes());
        out.push(0xBA); // mov edx, len
        out.extend_from_slice(&(marker_x.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0x8B, 0x3D]); // mov edi, ds:[addr_buf]
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xD7]); // call edi → mapped [int 0x80; ret]
                                              // emit END (from /init's own code)
        w(marker_end_addr, marker_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let code_addr = path_addr + path.len() as u32;
    let marker_fmap_addr = code_addr + codepayload.len() as u32;
    let marker_x_addr = marker_fmap_addr + marker_fmap.len() as u32;
    let marker_end_addr = marker_x_addr + marker_x.len() as u32;
    let fd_addr = marker_end_addr + marker_end.len() as u32;
    let addr_buf = fd_addr + 4;
    let code = build_code(
        path_addr,
        code_addr,
        marker_fmap_addr,
        marker_x_addr,
        marker_end_addr,
        fd_addr,
        addr_buf,
    );
    let fd_zeros = [0u8; 4];
    let addr_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            codepayload,
            marker_fmap,
            marker_x,
            marker_end,
            &fd_zeros,
            &addr_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init sets up thread-local storage exactly the
/// way glibc/musl do on i386 — via `set_thread_area` (syscall 243)
/// plus a `%gs`-relative read — then reports the result:
///
///   `[USERSPACE STA=<4 ret>ENT=<4 entry>TLS=<4 gs:0>][USERSPACE END]`
///
/// Sequence:
///   * fill a `struct user_desc` { entry_number=-1 (kernel picks),
///     base_addr=<tls block>, limit=0xFFFFF, flags=seg_32bit|
///     limit_in_pages|useable=0x51 },
///   * `set_thread_area(&ud)` — the kernel allocates a GDT TLS slot,
///     writes its base into that descriptor, and writes the chosen
///     entry_number back into the struct,
///   * build the selector `(entry<<3)|3`, `mov gs, ax`,
///   * `mov eax, gs:[0]` — reads the first word of the TLS block
///     through the freshly-installed segment base.
///
/// The TLS block's first word is a sentinel `0x12345678`; reading it
/// back through `%gs:0` proves the whole chain: set_thread_area built
/// a correct GDT descriptor, `mov gs` cached its base, and a
/// segment-relative load uses that base. This is the precise
/// mechanism every glibc/musl program relies on before `main`.
/// Asserts STA==0 and the `%gs:0` read equals the sentinel.
fn build_initramfs_set_thread_area() -> Vec<u8> {
    let m_sta: &[u8] = b"[USERSPACE STA=";
    let m_ent: &[u8] = b"ENT=";
    let m_tls: &[u8] = b"TLS=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const SENTINEL: u32 = 0x1234_5678;

    let build_code = |m_sta_addr: u32,
                      m_ent_addr: u32,
                      m_tls_addr: u32,
                      m_end_addr: u32,
                      ud_addr: u32,
                      sta_buf: u32,
                      gs_buf: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // set_thread_area(&ud) — sys 243
        out.extend_from_slice(&[0xB8, 0xF3, 0x00, 0x00, 0x00]); // mov eax, 243
        out.push(0xBB);
        out.extend_from_slice(&ud_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[sta_buf], eax (return code)
        out.extend_from_slice(&sta_buf.to_le_bytes());
        // selector = (entry_number << 3) | 3 ; load into gs
        out.push(0xA1); // mov eax, ds:[ud] (entry_number, written back)
        out.extend_from_slice(&ud_addr.to_le_bytes());
        out.extend_from_slice(&[0xC1, 0xE0, 0x03]); // shl eax, 3
        out.extend_from_slice(&[0x83, 0xC8, 0x03]); // or eax, 3
        out.extend_from_slice(&[0x8E, 0xE8]); // mov gs, ax
                                              // eax = gs:[0] — read the TLS block's first word.
        out.extend_from_slice(&[0x65, 0xA1, 0x00, 0x00, 0x00, 0x00]); // mov eax, gs:[0]
        out.push(0xA3); // mov ds:[gs_buf], eax
        out.extend_from_slice(&gs_buf.to_le_bytes());
        // emit STA=<ret> ENT=<entry> TLS=<gs:0> END
        w(m_sta_addr, m_sta.len() as u32, &mut out);
        w(sta_buf, 4, &mut out);
        w(m_ent_addr, m_ent.len() as u32, &mut out);
        w(ud_addr, 4, &mut out); // entry_number, written back by the kernel
        w(m_tls_addr, m_tls.len() as u32, &mut out);
        w(gs_buf, 4, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let m_sta_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_ent_addr = m_sta_addr + m_sta.len() as u32;
    let m_tls_addr = m_ent_addr + m_ent.len() as u32;
    let m_end_addr = m_tls_addr + m_tls.len() as u32;
    let ud_addr = m_end_addr + m_end.len() as u32;
    let tls_addr = ud_addr + 16; // user_desc is 16 bytes
    let sta_buf = tls_addr + 16; // tls block is 16 bytes
    let gs_buf = sta_buf + 4;
    let code = build_code(
        m_sta_addr, m_ent_addr, m_tls_addr, m_end_addr, ud_addr, sta_buf, gs_buf,
    );

    // struct user_desc: entry=-1, base=tls_addr, limit=0xFFFFF,
    // flags = seg_32bit(0x1)|limit_in_pages(0x10)|useable(0x40) = 0x51.
    let mut user_desc = Vec::with_capacity(16);
    user_desc.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    user_desc.extend_from_slice(&tls_addr.to_le_bytes());
    user_desc.extend_from_slice(&0x000F_FFFFu32.to_le_bytes());
    user_desc.extend_from_slice(&0x0000_0051u32.to_le_bytes());
    // TLS block: sentinel at offset 0, padded to 16 bytes.
    let mut tls_block = Vec::with_capacity(16);
    tls_block.extend_from_slice(&SENTINEL.to_le_bytes());
    tls_block.extend_from_slice(&[0u8; 12]);
    let sta_zeros = [0u8; 4];
    let gs_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            m_sta, m_ent, m_tls, m_end, &user_desc, &tls_block, &sta_zeros, &gs_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init proves `MAP_SHARED` anonymous memory is
/// shared across a `fork`. It maps one page
/// `PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS`, forks, the
/// child writes a sentinel word into the page and exits, and the
/// parent (after `waitpid`) reads the same page and reports it:
///
///   `[USERSPACE SHM=<4 bytes>][USERSPACE END]`
///
/// The decisive property: with `MAP_SHARED` the child and parent map
/// the *same* physical page, so the child's write is visible to the
/// parent. Under `MAP_PRIVATE`/COW it would NOT be — the parent
/// would still see zero. Reading back the child's sentinel therefore
/// pins shared-anonymous-memory semantics (the basis of POSIX shared
/// memory and threaded data sharing), distinct from the COW path the
/// fork/brk milestones exercise. The test asserts the parent read
/// the child's sentinel (`0x12345678`).
fn build_initramfs_shared_mmap() -> Vec<u8> {
    let m_shm: &[u8] = b"[USERSPACE SHM=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const SENTINEL: u32 = 0x1234_5678;

    let build_code = |m_shm_addr: u32, m_end_addr: u32, addr_buf: u32, shm_buf: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // addr = mmap2(NULL, 0x1000, PROT_R|W=3,
        //              MAP_SHARED|MAP_ANONYMOUS=0x21, -1, 0)
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]); // mov eax, 192
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]); // mov ecx, 0x1000
        out.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]); // mov edx, R|W
        out.extend_from_slice(&[0xBE, 0x21, 0x00, 0x00, 0x00]); // mov esi, 0x21
        out.extend_from_slice(&[0xBF, 0xFF, 0xFF, 0xFF, 0xFF]); // mov edi, -1
        out.extend_from_slice(&[0x31, 0xED]); // xor ebp, ebp
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[addr_buf], eax
        out.extend_from_slice(&addr_buf.to_le_bytes());
        // fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched)
                                              // ===== CHILD: write the sentinel into the shared page =====
        out.extend_from_slice(&[0x8B, 0x3D]); // mov edi, ds:[addr_buf]
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0xC7, 0x07]); // mov dword [edi], imm32
        out.extend_from_slice(&SENTINEL.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0)
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // ===== PARENT =====
        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;
        // waitpid(-1, NULL, 0) — wait for the child to write + exit
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // read the shared page: mov edi,[addr_buf]; mov eax,[edi]
        out.extend_from_slice(&[0x8B, 0x3D]);
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0x8B, 0x07]); // mov eax, [edi]
        out.push(0xA3); // mov ds:[shm_buf], eax
        out.extend_from_slice(&shm_buf.to_le_bytes());
        // emit SHM=<value> END
        w(m_shm_addr, m_shm.len() as u32, &mut out);
        w(shm_buf, 4, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0).len() as u32;
    let m_shm_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_end_addr = m_shm_addr + m_shm.len() as u32;
    let addr_buf = m_end_addr + m_end.len() as u32;
    let shm_buf = addr_buf + 4;
    let code = build_code(m_shm_addr, m_end_addr, addr_buf, shm_buf);
    let addr_zeros = [0u8; 4];
    let shm_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(&code, &[m_shm, m_end, &addr_zeros, &shm_zeros], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init exercises `sys_futex` WAIT/WAKE across a
/// shared page — the kernel primitive every pthread mutex/condvar is
/// built on. It maps a MAP_SHARED page (the futex word, init 0),
/// forks, and:
///   * child: `futex(&word, FUTEX_WAIT, 0, NULL)` — blocks because
///     *word == 0 and nothing changes it,
///   * parent: spins `futex(&word, FUTEX_WAKE, 1)` until it reports
///     it woke a waiter (return >= 1), then `waitpid`s the child.
///
/// Output: `[USERSPACE WOKE=<4 wait-ret>WAKE=<4 wake-ret>][USERSPACE END]`.
/// The WAKE return == 1 is the decisive proof: the kernel only
/// reports waking 1 task if the child was genuinely blocked in
/// FUTEX_WAIT and FUTEX_WAKE found and woke it — i.e. the full
/// kernel-side wait-queue handoff worked. The child's WAIT return ==
/// 0 confirms it returned via a wake (not -EAGAIN). The spin makes
/// the handoff deterministic regardless of fork scheduling order.
fn build_initramfs_futex() -> Vec<u8> {
    let m_woke: &[u8] = b"[USERSPACE WOKE=";
    let m_wake: &[u8] = b"WAKE=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const WAKE_SPIN_CAP: u32 = 0x0010_0000;

    let build_code = |m_woke_addr: u32,
                      m_wake_addr: u32,
                      m_end_addr: u32,
                      addr_buf: u32,
                      wait_ret: u32,
                      wake_ret: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // addr = mmap2(NULL, 0x1000, R|W, MAP_SHARED|ANON=0x21, -1, 0)
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBE, 0x21, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBF, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xED]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[addr_buf], eax
        out.extend_from_slice(&addr_buf.to_le_bytes());
        // fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched)

        // ===== CHILD: futex(&word, FUTEX_WAIT, 0, NULL) =====
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[addr_buf] (uaddr)
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0xF0, 0x00, 0x00, 0x00]); // mov eax, 240 (futex)
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (op = FUTEX_WAIT)
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (val = 0)
        out.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi (timeout = NULL)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wait_ret], eax
        out.extend_from_slice(&wait_ret.to_le_bytes());
        w(m_woke_addr, m_woke.len() as u32, &mut out);
        w(wait_ret, 4, &mut out);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0)
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);

        // ===== PARENT =====
        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;
        // ebp = spin cap
        out.push(0xBD); // mov ebp, imm32
        out.extend_from_slice(&WAKE_SPIN_CAP.to_le_bytes());
        let wake_loop = out.len();
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[addr_buf]
        out.extend_from_slice(&addr_buf.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0xF0, 0x00, 0x00, 0x00]); // mov eax, 240
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, 1 (FUTEX_WAKE)
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1 (wake up to 1)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wake_ret], eax
        out.extend_from_slice(&wake_ret.to_le_bytes());
        out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        let jg_at = out.len();
        out.extend_from_slice(&[0x7F, 0x00]); // jg wake_done (woke >=1) (patched)
        out.push(0x4D); // dec ebp
                        // jnz wake_loop (backward)
        out.push(0x75);
        let after = out.len() + 1;
        out.push(((wake_loop as i32) - (after as i32)) as u8);
        let wake_done = out.len();
        out[jg_at + 1] = (wake_done - (jg_at + 2)) as u8;
        // waitpid(-1, NULL, 0)
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // emit WAKE=<wake_ret> END
        w(m_wake_addr, m_wake.len() as u32, &mut out);
        w(wake_ret, 4, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0)
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let m_woke_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_wake_addr = m_woke_addr + m_woke.len() as u32;
    let m_end_addr = m_wake_addr + m_wake.len() as u32;
    let addr_buf = m_end_addr + m_end.len() as u32;
    let wait_ret = addr_buf + 4;
    let wake_ret = wait_ret + 4;
    let code = build_code(
        m_woke_addr,
        m_wake_addr,
        m_end_addr,
        addr_buf,
        wait_ret,
        wake_ret,
    );
    let z4 = [0u8; 4];
    let binary = make_init_elf32_safe(&code, &[m_woke, m_wake, m_end, &z4, &z4, &z4], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates a real thread with `clone(CLONE_VM)`
/// — the kernel mechanism behind `pthread_create`. It mmap's a stack
/// page, `clone`s a task that shares the address space, and the new
/// task writes a sentinel into a *normal data global* (no MAP_SHARED
/// needed — CLONE_VM means one address space). The parent busy-waits
/// on that global, then reaps the child.
///
///   `[USERSPACE THREAD=<4 bytes>][USERSPACE END]`
///
/// The decisive property: the parent reads the sentinel
/// (`0xABCDEF01`) the *cloned task* wrote to a shared global. Without
/// CLONE_VM the child would get a copy and the parent would read 0.
/// This pins shared-address-space thread creation: `clone` starting a
/// task on a caller-supplied stack with eax=0, both tasks running in
/// one mm. With TLS (`set_thread_area`) and futex already proven,
/// this completes the core threading triad.
fn build_initramfs_clone_thread() -> Vec<u8> {
    let m_thread: &[u8] = b"[USERSPACE THREAD=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const SENTINEL: u32 = 0xABCD_EF01;
    const SPIN_CAP: u32 = 0x0010_0000;

    let build_code =
        |m_thread_addr: u32, m_end_addr: u32, cstack_buf: u32, g_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(224);
            let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
                out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
                out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
                out.push(0xB9);
                out.extend_from_slice(&addr.to_le_bytes());
                out.push(0xBA);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&[0xCD, 0x80]);
            };
            // stack = mmap2(NULL, 0x1000, R|W, MAP_PRIVATE|ANON=0x22, -1, 0)
            out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0x31, 0xDB]);
            out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]);
            out.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBE, 0x22, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBF, 0xFF, 0xFF, 0xFF, 0xFF]);
            out.extend_from_slice(&[0x31, 0xED]);
            out.extend_from_slice(&[0xCD, 0x80]);
            // child stack top = base + 0x1000 (stacks grow down)
            out.extend_from_slice(&[0x05, 0x00, 0x10, 0x00, 0x00]); // add eax, 0x1000
            out.push(0xA3); // mov ds:[cstack_buf], eax
            out.extend_from_slice(&cstack_buf.to_le_bytes());
            // clone(CLONE_VM=0x100, child_stack, ptid=0, tls=0, ctid=0)
            // i386 reg order: ebx=flags, ecx=newsp, edx=ptid, esi=tls,
            // edi=ctid.
            out.extend_from_slice(&[0xB8, 0x78, 0x00, 0x00, 0x00]); // mov eax, 120 (clone)
            out.extend_from_slice(&[0xBB, 0x00, 0x01, 0x00, 0x00]); // mov ebx, CLONE_VM
            out.extend_from_slice(&[0x8B, 0x0D]); // mov ecx, ds:[cstack_buf]
            out.extend_from_slice(&cstack_buf.to_le_bytes());
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
            out.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi
            out.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
            out.extend_from_slice(&[0xCD, 0x80]);
            out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
            let jnz_at = out.len();
            out.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched)

            // ===== CHILD (eax==0, running on child_stack) =====
            // g = SENTINEL — visible to the parent via the shared mm.
            out.extend_from_slice(&[0xC7, 0x05]); // mov dword ds:[g], imm32
            out.extend_from_slice(&g_addr.to_le_bytes());
            out.extend_from_slice(&SENTINEL.to_le_bytes());
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0) — per-task
            out.extend_from_slice(&[0x31, 0xDB]);
            out.extend_from_slice(&[0xCD, 0x80]);

            // ===== PARENT =====
            let parent_start = out.len();
            let disp = (parent_start - (jnz_at + 2)) as i32;
            assert!(
                (-128..=127).contains(&disp),
                "child block too large for jnz disp8 (got {disp})"
            );
            out[jnz_at + 1] = disp as u8;
            out.push(0xBD); // mov ebp, SPIN_CAP
            out.extend_from_slice(&SPIN_CAP.to_le_bytes());
            let spin = out.len();
            out.push(0xA1); // mov eax, ds:[g]
            out.extend_from_slice(&g_addr.to_le_bytes());
            out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
            let jnz_got_at = out.len();
            out.extend_from_slice(&[0x75, 0x00]); // jnz got (the child wrote g) (patched)
            out.push(0x4D); // dec ebp
            out.push(0x75); // jnz spin (backward)
            let after = out.len() + 1;
            out.push(((spin as i32) - (after as i32)) as u8);
            let got = out.len();
            out[jnz_got_at + 1] = (got - (jnz_got_at + 2)) as u8;
            // waitpid(-1, NULL, 0)
            out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
            out.extend_from_slice(&[0x31, 0xC9]);
            out.extend_from_slice(&[0x31, 0xD2]);
            out.extend_from_slice(&[0xCD, 0x80]);
            // emit THREAD=<g> END
            w(m_thread_addr, m_thread.len() as u32, &mut out);
            w(g_addr, 4, &mut out);
            w(m_end_addr, m_end.len() as u32, &mut out);
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // exit(0)
            out.extend_from_slice(&[0x31, 0xDB]);
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };

    let code_len = build_code(0, 0, 0, 0).len() as u32;
    let m_thread_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_end_addr = m_thread_addr + m_thread.len() as u32;
    let cstack_buf = m_end_addr + m_end.len() as u32;
    let g_addr = cstack_buf + 4;
    let code = build_code(m_thread_addr, m_end_addr, cstack_buf, g_addr);
    let z4 = [0u8; 4];
    let binary = make_init_elf32_safe(&code, &[m_thread, m_end, &z4, &z4], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init exercises the AF_UNIX socket layer:
/// `socketpair(AF_UNIX, SOCK_STREAM, 0, &sv)` (the direct i386
/// syscall 360), then `write(sv[0], "SOCK", 4)` and
/// `read(sv[1], buf, 4)`, reporting every return plus the bytes:
///
///   `[USERSPACE SPR=<4>S0=<4>S1=<4>WR=<4>RD=<4>BUF=<4>][USERSPACE END]`
///
/// This is the first test to touch the kernel socket subsystem
/// (`net/socket.c` + `net/unix/af_unix.c`) — an entirely separate
/// layer from pipes/files. A connected AF_UNIX stream pair is the
/// IPC backbone of D-Bus, X11, systemd, etc. The test asserts the
/// pair allocated two distinct fds, both transfers returned 4, and
/// the bytes read back equal what was written (`"SOCK"`).
fn build_initramfs_socketpair() -> Vec<u8> {
    let msg: &[u8] = b"SOCK";
    let m_sp: &[u8] = b"[USERSPACE SPR=";
    let m_s0: &[u8] = b"S0=";
    let m_s1: &[u8] = b"S1=";
    let m_wr: &[u8] = b"WR=";
    let m_rd: &[u8] = b"RD=";
    let m_buf: &[u8] = b"BUF=";
    let m_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |msg_addr: u32,
                      m_sp_addr: u32,
                      m_s0_addr: u32,
                      m_s1_addr: u32,
                      m_wr_addr: u32,
                      m_rd_addr: u32,
                      m_buf_addr: u32,
                      m_end_addr: u32,
                      sv_addr: u32,
                      buf_addr: u32,
                      sp_ret_addr: u32,
                      wr_ret_addr: u32,
                      rd_ret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(384);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // socketpair(AF_UNIX=1, SOCK_STREAM=1, 0, &sv) — sys 360
        out.extend_from_slice(&[0xB8, 0x68, 0x01, 0x00, 0x00]); // mov eax, 360
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, AF_UNIX
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, SOCK_STREAM
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (protocol 0)
        out.push(0xBE); // mov esi, &sv
        out.extend_from_slice(&sv_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[sp_ret], eax
        out.extend_from_slice(&sp_ret_addr.to_le_bytes());
        // write(sv[0], msg, 4)
        out.push(0xA1); // mov eax, ds:[sv]
        out.extend_from_slice(&sv_addr.to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]); // mov edx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wr_ret], eax
        out.extend_from_slice(&wr_ret_addr.to_le_bytes());
        // read(sv[1], buf, 4)
        out.push(0xA1); // mov eax, ds:[sv+4]
        out.extend_from_slice(&(sv_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]); // mov edx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[rd_ret], eax
        out.extend_from_slice(&rd_ret_addr.to_le_bytes());
        // emit everything
        w(m_sp_addr, m_sp.len() as u32, &mut out);
        w(sp_ret_addr, 4, &mut out);
        w(m_s0_addr, m_s0.len() as u32, &mut out);
        w(sv_addr, 4, &mut out);
        w(m_s1_addr, m_s1.len() as u32, &mut out);
        w(sv_addr + 4, 4, &mut out);
        w(m_wr_addr, m_wr.len() as u32, &mut out);
        w(wr_ret_addr, 4, &mut out);
        w(m_rd_addr, m_rd.len() as u32, &mut out);
        w(rd_ret_addr, 4, &mut out);
        w(m_buf_addr, m_buf.len() as u32, &mut out);
        w(buf_addr, 4, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let msg_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_sp_addr = msg_addr + msg.len() as u32;
    let m_s0_addr = m_sp_addr + m_sp.len() as u32;
    let m_s1_addr = m_s0_addr + m_s0.len() as u32;
    let m_wr_addr = m_s1_addr + m_s1.len() as u32;
    let m_rd_addr = m_wr_addr + m_wr.len() as u32;
    let m_buf_addr = m_rd_addr + m_rd.len() as u32;
    let m_end_addr = m_buf_addr + m_buf.len() as u32;
    let sv_addr = m_end_addr + m_end.len() as u32;
    let buf_addr = sv_addr + 8;
    let sp_ret_addr = buf_addr + 4;
    let wr_ret_addr = sp_ret_addr + 4;
    let rd_ret_addr = wr_ret_addr + 4;
    let code = build_code(
        msg_addr,
        m_sp_addr,
        m_s0_addr,
        m_s1_addr,
        m_wr_addr,
        m_rd_addr,
        m_buf_addr,
        m_end_addr,
        sv_addr,
        buf_addr,
        sp_ret_addr,
        wr_ret_addr,
        rd_ret_addr,
    );
    let sv_zeros = [0u8; 8];
    let z4 = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            msg, m_sp, m_s0, m_s1, m_wr, m_rd, m_buf, m_end, &sv_zeros, &z4, &z4, &z4, &z4,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init drives the `epoll` readiness subsystem:
/// it pipes a byte to make a read end ready, then
/// `epoll_create1` → `epoll_ctl(ADD, pipe_read, {EPOLLIN, cookie})`
/// → `epoll_wait`, reporting each return plus the readied event's
/// `events` mask and 64-bit `data` cookie:
///
///   `[USERSPACE EPC=<4>CTL=<4>WAIT=<4>EVT=<4>DAT=<8>][USERSPACE END]`
///
/// This exercises a new kernel subsystem (`fs/eventpoll.c`) and the
/// packed 12-byte `struct epoll_event` (u32 events + u64 data, no
/// padding on x86). The decisive proof is the 64-bit `data` cookie
/// (`0x0000CAFEDEADBEEF`) round-tripping: it is stored inside the
/// kernel by `epoll_ctl` and copied back out by `epoll_wait`, so a
/// match proves the registration + readiness-report path end to end.
/// epoll is the foundation of every modern event loop (nginx, Node).
fn build_initramfs_epoll() -> Vec<u8> {
    let msg: &[u8] = b"E";
    let m_epc: &[u8] = b"[USERSPACE EPC=";
    let m_ctl: &[u8] = b"CTL=";
    let m_wait: &[u8] = b"WAIT=";
    let m_evt: &[u8] = b"EVT=";
    let m_dat: &[u8] = b"DAT=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const COOKIE: u64 = 0x0000_CAFE_DEAD_BEEF;

    #[allow(clippy::too_many_arguments)]
    let build_code = |msg_addr: u32,
                      m_epc_addr: u32,
                      m_ctl_addr: u32,
                      m_wait_addr: u32,
                      m_evt_addr: u32,
                      m_dat_addr: u32,
                      m_end_addr: u32,
                      fds_addr: u32,
                      ev_in_addr: u32,
                      ev_out_addr: u32,
                      ep_buf: u32,
                      ctl_ret: u32,
                      wait_ret: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(384);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // pipe2(&fds, 0)
        out.extend_from_slice(&[0xB8, 0x4B, 0x01, 0x00, 0x00]); // mov eax, 331
        out.push(0xBB);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(fds[1], msg, 1) — make the read end EPOLLIN-ready
        out.push(0xA1);
        out.extend_from_slice(&(fds_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // ep = epoll_create1(0) — sys 329
        out.extend_from_slice(&[0xB8, 0x49, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[ep_buf], eax
        out.extend_from_slice(&ep_buf.to_le_bytes());
        // epoll_ctl(ep, EPOLL_CTL_ADD=1, fds[0], &ev_in) — sys 255
        out.extend_from_slice(&[0xB8, 0xFF, 0x00, 0x00, 0x00]); // mov eax, 255
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[ep_buf]
        out.extend_from_slice(&ep_buf.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, EPOLL_CTL_ADD
        out.extend_from_slice(&[0x8B, 0x15]); // mov edx, ds:[fds] (fds[0])
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.push(0xBE); // mov esi, &ev_in
        out.extend_from_slice(&ev_in_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[ctl_ret], eax
        out.extend_from_slice(&ctl_ret.to_le_bytes());
        // epoll_wait(ep, &ev_out, 1, 0) — sys 256
        out.extend_from_slice(&[0xB8, 0x00, 0x01, 0x00, 0x00]); // mov eax, 256
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[ep_buf]
        out.extend_from_slice(&ep_buf.to_le_bytes());
        out.push(0xB9); // mov ecx, &ev_out
        out.extend_from_slice(&ev_out_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1 (maxevents)
        out.extend_from_slice(&[0x31, 0xF6]); // xor esi, esi (timeout 0)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wait_ret], eax
        out.extend_from_slice(&wait_ret.to_le_bytes());
        // emit EPC CTL WAIT EVT(events @ ev_out+0) DAT(data @ ev_out+4) END
        w(m_epc_addr, m_epc.len() as u32, &mut out);
        w(ep_buf, 4, &mut out);
        w(m_ctl_addr, m_ctl.len() as u32, &mut out);
        w(ctl_ret, 4, &mut out);
        w(m_wait_addr, m_wait.len() as u32, &mut out);
        w(wait_ret, 4, &mut out);
        w(m_evt_addr, m_evt.len() as u32, &mut out);
        w(ev_out_addr, 4, &mut out);
        w(m_dat_addr, m_dat.len() as u32, &mut out);
        w(ev_out_addr + 4, 8, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let msg_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_epc_addr = msg_addr + msg.len() as u32;
    let m_ctl_addr = m_epc_addr + m_epc.len() as u32;
    let m_wait_addr = m_ctl_addr + m_ctl.len() as u32;
    let m_evt_addr = m_wait_addr + m_wait.len() as u32;
    let m_dat_addr = m_evt_addr + m_evt.len() as u32;
    let m_end_addr = m_dat_addr + m_dat.len() as u32;
    let fds_addr = m_end_addr + m_end.len() as u32;
    let ev_in_addr = fds_addr + 8;
    let ev_out_addr = ev_in_addr + 12; // packed struct epoll_event = 12 bytes
    let ep_buf = ev_out_addr + 12;
    let ctl_ret = ep_buf + 4;
    let wait_ret = ctl_ret + 4;
    let code = build_code(
        msg_addr,
        m_epc_addr,
        m_ctl_addr,
        m_wait_addr,
        m_evt_addr,
        m_dat_addr,
        m_end_addr,
        fds_addr,
        ev_in_addr,
        ev_out_addr,
        ep_buf,
        ctl_ret,
        wait_ret,
    );
    // ev_in: events=EPOLLIN(1) at +0, data=COOKIE at +4 (packed).
    let mut ev_in = Vec::with_capacity(12);
    ev_in.extend_from_slice(&1u32.to_le_bytes());
    ev_in.extend_from_slice(&COOKIE.to_le_bytes());
    let fds_zeros = [0u8; 8];
    let ev_out_zeros = [0u8; 12];
    let z4 = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            msg,
            m_epc,
            m_ctl,
            m_wait,
            m_evt,
            m_dat,
            m_end,
            &fds_zeros,
            &ev_in,
            &ev_out_zeros,
            &z4,
            &z4,
            &z4,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init exercises `eventfd` — the lightweight
/// 64-bit counter fd that pairs with epoll to wake event loops. It
/// `eventfd2(0, 0)`s a counter fd, `write`s the 8-byte value 42 to
/// add to the counter, then `read`s the 8-byte counter back (which
/// returns the count and resets it to 0):
///
///   `[USERSPACE EFD=<4>WR=<4>RD=<4>CNT=<8>][USERSPACE END]`
///
/// Pins the eventfd subsystem (`fs/eventfd.c`) and its strict 8-byte
/// counter read/write protocol. The test asserts a valid fd, both
/// transfers moved 8 bytes, and the counter read back equals the 42
/// that was written.
fn build_initramfs_eventfd() -> Vec<u8> {
    let m_efd: &[u8] = b"[USERSPACE EFD=";
    let m_wr: &[u8] = b"WR=";
    let m_rd: &[u8] = b"RD=";
    let m_cnt: &[u8] = b"CNT=";
    let m_end: &[u8] = b"][USERSPACE END]\n";
    const ADD_VALUE: u64 = 42;

    #[allow(clippy::too_many_arguments)]
    let build_code = |m_efd_addr: u32,
                      m_wr_addr: u32,
                      m_rd_addr: u32,
                      m_cnt_addr: u32,
                      m_end_addr: u32,
                      val8_addr: u32,
                      efd_buf: u32,
                      wr_ret: u32,
                      rd_ret: u32,
                      buf8_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // efd = eventfd2(0, 0) — sys 328
        out.extend_from_slice(&[0xB8, 0x48, 0x01, 0x00, 0x00]); // mov eax, 328
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (initval 0)
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (flags 0)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[efd_buf], eax
        out.extend_from_slice(&efd_buf.to_le_bytes());
        // write(efd, val8, 8) — add 42 to the counter
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[efd_buf]
        out.extend_from_slice(&efd_buf.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&val8_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wr_ret], eax
        out.extend_from_slice(&wr_ret.to_le_bytes());
        // read(efd, buf8, 8) — drain the counter (returns 42, resets to 0)
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[efd_buf]
        out.extend_from_slice(&efd_buf.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3
        out.push(0xB9);
        out.extend_from_slice(&buf8_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]); // mov edx, 8
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[rd_ret], eax
        out.extend_from_slice(&rd_ret.to_le_bytes());
        // emit EFD=<efd> WR=<wr> RD=<rd> CNT=<buf8> END
        w(m_efd_addr, m_efd.len() as u32, &mut out);
        w(efd_buf, 4, &mut out);
        w(m_wr_addr, m_wr.len() as u32, &mut out);
        w(wr_ret, 4, &mut out);
        w(m_rd_addr, m_rd.len() as u32, &mut out);
        w(rd_ret, 4, &mut out);
        w(m_cnt_addr, m_cnt.len() as u32, &mut out);
        w(buf8_addr, 8, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let m_efd_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_wr_addr = m_efd_addr + m_efd.len() as u32;
    let m_rd_addr = m_wr_addr + m_wr.len() as u32;
    let m_cnt_addr = m_rd_addr + m_rd.len() as u32;
    let m_end_addr = m_cnt_addr + m_cnt.len() as u32;
    let val8_addr = m_end_addr + m_end.len() as u32;
    let efd_buf = val8_addr + 8;
    let wr_ret = efd_buf + 4;
    let rd_ret = wr_ret + 4;
    let buf8_addr = rd_ret + 4;
    let code = build_code(
        m_efd_addr, m_wr_addr, m_rd_addr, m_cnt_addr, m_end_addr, val8_addr, efd_buf, wr_ret,
        rd_ret, buf8_addr,
    );
    let val8 = ADD_VALUE.to_le_bytes();
    let efd_z = [0u8; 4];
    let buf8_z = [0u8; 8];
    let binary = make_init_elf32_safe(
        &code,
        &[
            m_efd, m_wr, m_rd, m_cnt, m_end, &val8, &efd_z, &efd_z, &efd_z, &buf8_z,
        ],
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

/// Build a cpio whose /init creates `/a` with 7 bytes "RENDATA",
/// calls `sys_rename("/a", "/b")` (syscall 38), then opens `/b`
/// and reads the content back. Writes the 7 bytes between
/// `[USERSPACE RENAMED=…][USERSPACE END]`. Test asserts the
/// round-trip via the NEW path equals `b"RENDATA"` — proves
/// rename moved the inode (not copied) and the original content
/// is reachable through the new name. If rename had failed, /b
/// wouldn't exist and the open + read would have left buf as
/// zeros (or read would have returned -ENOENT and buf is
/// uninitialized garbage).
fn build_initramfs_rename() -> Vec<u8> {
    let old_path: &[u8] = b"/a\0";
    let new_path: &[u8] = b"/b\0";
    let payload: &[u8] = b"RENDATA"; // 7 bytes
    let marker_pre: &[u8] = b"[USERSPACE RENAMED=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |old_path_addr: u32,
                      new_path_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open(old_path, O_CREAT|O_WRONLY=0x41, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]); // mov eax, 5
        out.push(0xBB);
        out.extend_from_slice(&old_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[fd] = eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 7)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_rename(old_path, new_path) — sys 38
        out.extend_from_slice(&[0xB8, 0x26, 0x00, 0x00, 0x00]); // mov eax, 38
        out.push(0xBB);
        out.extend_from_slice(&old_path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&new_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd = sys_open(new_path, O_RDONLY, 0)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&new_path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_read(fd, buf, 7)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x07, 0x00, 0x00, 0x00]);
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
        // write(1, buf, 7)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x07, 0x00, 0x00, 0x00]);
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
    let old_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let new_path_addr = old_path_addr + old_path.len() as u32;
    let payload_addr = new_path_addr + new_path.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let buf_addr = fd_addr + 4;
    let code = build_code(
        old_path_addr,
        new_path_addr,
        payload_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let buf_zeros = [0u8; 7];
    let binary = make_init_elf32_safe(
        &code,
        &[
            old_path,
            new_path,
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

/// Build a cpio whose /init creates `/probe` with mode 0o644,
/// then calls `sys_chmod("/probe", 0o600)` (syscall 15), stats
/// it, and reads `st_mode` (4 bytes at offset 16 in struct
/// stat64). Writes the mode between
/// `[USERSPACE MODE=…][USERSPACE END]`. Test asserts mode
/// equals `S_IFREG | 0o600 = 0o100600 = 0x81C0` — proves the
/// kernel updated the inode's mode bits AND stat reflects the
/// new value.
fn build_initramfs_chmod() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let marker_pre: &[u8] = b"[USERSPACE MODE=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      statbuf_addr: u32,
                      mode_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // fd = sys_open(path, O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]); // mov edx, 0o644
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_chmod(path, 0o600) — sys 15
        out.extend_from_slice(&[0xB8, 0x0F, 0x00, 0x00, 0x00]); // mov eax, 15
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x80, 0x01, 0x00, 0x00]); // mov ecx, 0o600 = 0x180
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_stat64(path, &statbuf)
        out.extend_from_slice(&[0xB8, 0xC3, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&statbuf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // Read st_mode (4 bytes at offset 16 in struct stat64).
        out.push(0xA1);
        out.extend_from_slice(&(statbuf_addr + 16).to_le_bytes());
        out.push(0xA3);
        out.extend_from_slice(&mode_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, mode_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&mode_buf_addr.to_le_bytes());
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
    let mode_buf_addr = statbuf_addr + 100;
    let code = build_code(
        path_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        statbuf_addr,
        mode_buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let statbuf_zeros = [0u8; 100];
    let mode_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            marker_pre,
            marker_post,
            &fd_zeros,
            &statbuf_zeros,
            &mode_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates `/probe`, calls
/// `sys_access("/probe", F_OK=0)` (syscall 33) — should return 0
/// since the file exists. Then unlinks `/probe` and accesses
/// again — should return -ENOENT. /init writes both returns
/// between `[USERSPACE PRE=…POST=…][USERSPACE END]`. Test
/// asserts PRE=0 and POST=0xFFFFFFFE (-2 sign-extended).
fn build_initramfs_access() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let marker_pre: &[u8] = b"[USERSPACE PRE=";
    let marker_mid: &[u8] = b"POST=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      fd_addr: u32,
                      pre_buf_addr: u32,
                      post_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open(path, O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_access(path, F_OK=0) — sys 33
        out.extend_from_slice(&[0xB8, 0x21, 0x00, 0x00, 0x00]); // mov eax, 33
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (F_OK)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[pre_buf], eax
        out.extend_from_slice(&pre_buf_addr.to_le_bytes());
        // sys_unlink(path) — sys 10
        out.extend_from_slice(&[0xB8, 0x0A, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_access(path, F_OK) — should now return -ENOENT
        out.extend_from_slice(&[0xB8, 0x21, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[post_buf], eax
        out.extend_from_slice(&post_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, pre_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&pre_buf_addr.to_le_bytes());
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
        // write(1, post_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&post_buf_addr.to_le_bytes());
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

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_pre_addr = path_addr + path.len() as u32;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_end_addr = marker_mid_addr + marker_mid.len() as u32;
    let fd_addr = marker_end_addr + marker_end.len() as u32;
    let pre_buf_addr = fd_addr + 4;
    let post_buf_addr = pre_buf_addr + 4;
    let code = build_code(
        path_addr,
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        fd_addr,
        pre_buf_addr,
        post_buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let pre_zeros = [0u8; 4];
    let post_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path,
            marker_pre,
            marker_mid,
            marker_end,
            &fd_zeros,
            &pre_zeros,
            &post_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init opens a file, calls
/// `sys_fcntl(fd, F_SETFD=2, 1)` to mark it close-on-exec, then
/// `sys_fcntl(fd, F_GETFD=1, 0)` to read the flag back. Writes
/// the 4-byte returned value between
/// `[USERSPACE FD_FLAGS=…][USERSPACE END]`. Test asserts the
/// value equals `FD_CLOEXEC = 1` — proves the kernel stored the
/// fd-flag in the file-descriptor table entry AND returned it
/// on read.
fn build_initramfs_fcntl() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let marker_pre: &[u8] = b"[USERSPACE FD_FLAGS=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      flags_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // fd = sys_open(path, O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[fd] = eax
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_fcntl(fd, F_SETFD=2, FD_CLOEXEC=1) — sys 55
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x37, 0x00, 0x00, 0x00]); // mov eax, 55
        out.extend_from_slice(&[0xB9, 0x02, 0x00, 0x00, 0x00]); // mov ecx, 2 (F_SETFD)
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1 (FD_CLOEXEC)
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_fcntl(fd, F_GETFD=1, 0)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x37, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, 1 (F_GETFD)
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[flags_buf] = eax (fd flags returned)
        out.extend_from_slice(&flags_buf_addr.to_le_bytes());
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
        // write(1, flags_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&flags_buf_addr.to_le_bytes());
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

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_pre_addr = path_addr + path.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let flags_buf_addr = fd_addr + 4;
    let code = build_code(
        path_addr,
        marker_pre_addr,
        marker_post_addr,
        fd_addr,
        flags_buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let flags_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[path, marker_pre, marker_post, &fd_zeros, &flags_zeros],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init mmap's an anonymous page R+W,
/// writes 0x42 to it, calls `sys_mprotect(addr, 4096, PROT_READ)`
/// (syscall 125) to drop write permission, then reads the byte
/// back. /init writes `mprotect`'s eax return AND the byte
/// between `[USERSPACE MPROT_RET=<4>BYTE=<1>][USERSPACE END]`.
/// Test asserts mprotect_ret == 0 AND byte == 0x42.
///
/// What this pins beyond mmap: kernel's per-VMA protection
/// state-machine. After mprotect, the VMA's `vm_flags` has
/// VM_WRITE cleared. We don't test write-after-mprotect
/// (would SIGSEGV /init); we test the soft case — mprotect
/// returns 0 AND data is preserved.
fn build_initramfs_mprotect() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE MPROT_RET=";
    let marker_mid: &[u8] = b"BYTE=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_end_addr: u32,
                      ret_buf_addr: u32,
                      byte_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        // sys_mmap2(NULL, 0x1000, R|W=3, ANON|PRIVATE=0x22, -1, 0)
        out.extend_from_slice(&[0xB8, 0xC0, 0x00, 0x00, 0x00]); // mov eax, 192
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]); // mov ecx, 0x1000
        out.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]); // mov edx, 3
        out.extend_from_slice(&[0xBE, 0x22, 0x00, 0x00, 0x00]); // mov esi, 0x22
        out.extend_from_slice(&[0xBF, 0xFF, 0xFF, 0xFF, 0xFF]); // mov edi, -1
        out.extend_from_slice(&[0x31, 0xED]); // xor ebp, ebp
        out.extend_from_slice(&[0xCD, 0x80]);
        // Stash mmap addr in esi (preserved across syscalls).
        out.extend_from_slice(&[0x89, 0xC6]); // mov esi, eax
                                              // Write 0x42 into the page: mov byte ptr [esi], 0x42
        out.extend_from_slice(&[0xC6, 0x06, 0x42]);
        // sys_mprotect(addr, 4096, PROT_READ=1) — sys 125
        out.extend_from_slice(&[0xB8, 0x7D, 0x00, 0x00, 0x00]); // mov eax, 125
        out.extend_from_slice(&[0x89, 0xF3]); // mov ebx, esi
        out.extend_from_slice(&[0xB9, 0x00, 0x10, 0x00, 0x00]); // mov ecx, 4096
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, PROT_READ
        out.extend_from_slice(&[0xCD, 0x80]);
        // Store mprotect's eax return.
        out.push(0xA3);
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        // Read the byte back from the (now RO) page.
        out.extend_from_slice(&[0x8A, 0x06]); // mov al, [esi]
        out.push(0xA2);
        out.extend_from_slice(&byte_buf_addr.to_le_bytes());
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
        // write(1, marker_mid, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_mid_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_mid.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, byte_buf, 1)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&byte_buf_addr.to_le_bytes());
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
    let ret_buf_addr = marker_end_addr + marker_end.len() as u32;
    let byte_buf_addr = ret_buf_addr + 4;
    let code = build_code(
        marker_pre_addr,
        marker_mid_addr,
        marker_end_addr,
        ret_buf_addr,
        byte_buf_addr,
    );
    let ret_zeros = [0u8; 4];
    let byte_zeros = [0u8; 1];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_mid, marker_end, &ret_zeros, &byte_zeros],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_sysinfo(&buf)` (syscall
/// 116) and reads `uptime` (first field, 4 bytes) of struct
/// sysinfo back out. Writes the 4-byte uptime between
/// `[USERSPACE UPTIME=…][USERSPACE END]`. Test asserts the value
/// is positive — proves the kernel filled the sysinfo struct
/// AND that its internal uptime counter has advanced past boot.
fn build_initramfs_sysinfo() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE UPTIME=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32,
                      marker_post_addr: u32,
                      buf_addr: u32,
                      uptime_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // sys_sysinfo(&buf) — sys 116
        out.extend_from_slice(&[0xB8, 0x74, 0x00, 0x00, 0x00]); // mov eax, 116
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov eax, ds:[buf+0] (uptime) → ds:[uptime_buf]
        out.push(0xA1);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.push(0xA3);
        out.extend_from_slice(&uptime_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, uptime_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&uptime_buf_addr.to_le_bytes());
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

    let code_len = build_code(0, 0, 0, 0).len() as u32;
    let marker_pre_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    // struct sysinfo is 64 bytes on i386
    let uptime_buf_addr = buf_addr + 64;
    let code = build_code(marker_pre_addr, marker_post_addr, buf_addr, uptime_buf_addr);
    let buf_zeros = [0u8; 64];
    let uptime_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_pre, marker_post, &buf_zeros, &uptime_zeros],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init installs a SIGUSR1 handler via
/// `sys_rt_sigaction` (syscall 174), sends SIGUSR1 to itself via
/// `sys_kill(getpid(), SIGUSR1)` (syscall 37), and exits. The
/// handler — embedded as a function inside /init's code segment —
/// writes `[USERSPACE HANDLER]\n` then returns via `ret`, which
/// pops the kernel-stashed return address pointing at the
/// `sa_restorer` stub also embedded in the code. The restorer
/// calls `sys_rt_sigreturn` (syscall 173) which restores the
/// pre-signal ucontext, and main resumes past the kill, writes
/// `[USERSPACE DONE]\n` and exits.
///
/// What this pins:
///   - sys_rt_sigaction stores the handler in the task's
///     `sighand_struct`
///   - sys_kill signals self; kernel queues + delivers
///   - signal delivery sets up sigframe on user stack + jumps
///     to handler with restorer-return-addr pushed
///   - sys_rt_sigreturn restores ucontext correctly so main
///     resumes
///
/// Failure modes (handled by test diagnostics): if HANDLER
/// marker never appears, signal delivery is broken; if DONE
/// marker never appears, sigreturn path is broken (handler ran
/// but main never resumed).
fn build_initramfs_signal() -> Vec<u8> {
    let main_marker: &[u8] = b"[USERSPACE DONE]\n";
    let handler_marker: &[u8] = b"[USERSPACE HANDLER]\n";

    // Main code: sigaction + getpid + kill + write DONE + exit.
    let main_code_builder =
        |sigact_addr: u32, pid_buf_addr: u32, main_marker_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(96);
            // sys_rt_sigaction(SIGUSR1=10, &sigact, NULL, 8)
            out.extend_from_slice(&[0xB8, 0xAE, 0x00, 0x00, 0x00]); // mov eax, 174
            out.extend_from_slice(&[0xBB, 0x0A, 0x00, 0x00, 0x00]); // ebx = 10
            out.push(0xB9);
            out.extend_from_slice(&sigact_addr.to_le_bytes());
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (oldact NULL)
            out.extend_from_slice(&[0xBE, 0x08, 0x00, 0x00, 0x00]); // esi = 8 (sigsetsize)
            out.extend_from_slice(&[0xCD, 0x80]);
            // sys_getpid
            out.extend_from_slice(&[0xB8, 0x14, 0x00, 0x00, 0x00]); // mov eax, 20
            out.extend_from_slice(&[0xCD, 0x80]);
            out.push(0xA3); // ds:[pid_buf] = eax
            out.extend_from_slice(&pid_buf_addr.to_le_bytes());
            // sys_kill(pid, SIGUSR1=10)
            out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[pid_buf]
            out.extend_from_slice(&pid_buf_addr.to_le_bytes());
            out.extend_from_slice(&[0xB8, 0x25, 0x00, 0x00, 0x00]); // mov eax, 37
            out.extend_from_slice(&[0xB9, 0x0A, 0x00, 0x00, 0x00]); // mov ecx, 10
            out.extend_from_slice(&[0xCD, 0x80]);
            // After kill returns (and the handler has run + sigreturn'd):
            // write(1, main_marker, len)
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&main_marker_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&(main_marker.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // exit(0)
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0x31, 0xDB]);
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };
    // Handler code: write the HANDLER marker, then call
    // sys_exit(0) directly. This keeps the test a minimal one-way
    // signal-DELIVERY pin (rt_sigaction stored the handler, kill
    // queued the signal, kernel jumped to the handler's address),
    // independent of the sigreturn round-trip. The full round-trip
    // — handler returning via `ret` → restorer → rt_sigreturn →
    // resume — is covered separately by
    // `linux_userspace_sigreturn_milestone`
    // (`build_initramfs_signal_rt`).
    let handler_code_builder = |handler_marker_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&handler_marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(handler_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    // Restorer: sys_rt_sigreturn (syscall 173).
    let restorer_code_builder = || -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&[0xB8, 0xAD, 0x00, 0x00, 0x00]); // mov eax, 173
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let main_len = main_code_builder(0, 0, 0).len() as u32;
    let handler_len = handler_code_builder(0).len() as u32;
    let restorer_len = restorer_code_builder().len() as u32;
    // Code layout: main, handler, restorer.
    let main_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET;
    let handler_addr = main_addr + main_len;
    let restorer_addr = handler_addr + handler_len;
    let total_code_len = main_len + handler_len + restorer_len;
    // Data follows code.
    let sigact_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + total_code_len;
    let pid_buf_addr = sigact_addr + 20; // struct sigaction = 20 bytes on i386
    let main_marker_addr = pid_buf_addr + 4;
    let handler_marker_addr = main_marker_addr + main_marker.len() as u32;

    // Assemble code.
    let main = main_code_builder(sigact_addr, pid_buf_addr, main_marker_addr);
    let handler = handler_code_builder(handler_marker_addr);
    let restorer = restorer_code_builder();
    let mut code = Vec::with_capacity(total_code_len as usize);
    code.extend_from_slice(&main);
    code.extend_from_slice(&handler);
    code.extend_from_slice(&restorer);

    // Build sigact struct (sa_handler, sa_flags, sa_restorer, sa_mask[8]).
    let mut sigact_bytes = Vec::with_capacity(20);
    sigact_bytes.extend_from_slice(&handler_addr.to_le_bytes());
    sigact_bytes.extend_from_slice(&0x04000000u32.to_le_bytes()); // SA_RESTORER
    sigact_bytes.extend_from_slice(&restorer_addr.to_le_bytes());
    sigact_bytes.extend_from_slice(&[0u8; 8]); // sa_mask

    let pid_zeros = [0u8; 4];

    let binary = make_init_elf32_safe(
        &code,
        &[&sigact_bytes, &pid_zeros, main_marker, handler_marker],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Round-trip signal /init: identical to `build_initramfs_signal`
/// except (a) the handler RETURNS via `ret` instead of exiting, and
/// (b) `sa_flags` sets `SA_SIGINFO` so the kernel builds an
/// rt_sigframe matching the `rt_sigreturn(173)` restorer. This
/// exercises the full sigreturn path the one-way milestone skips:
///   * kernel `setup_rt_frame` pushes the rt_sigframe (pretcode =
///     `sa_restorer`, plus the saved sigcontext in `uc.uc_mcontext`)
///     onto the user stack,
///   * handler `ret` pops pretcode → jumps to the restorer stub,
///   * restorer calls `sys_rt_sigreturn(173)`,
///   * kernel restores the pre-signal context from the frame and
///     resumes `main`, which writes `[USERSPACE DONE]`.
///
/// The `SA_SIGINFO` bit is load-bearing — see the long note at the
/// `sa_flags` assignment below and on `linux_userspace_sigreturn_milestone`.
fn build_initramfs_signal_rt() -> Vec<u8> {
    let main_marker: &[u8] = b"[USERSPACE DONE]\n";
    let handler_marker: &[u8] = b"[USERSPACE HANDLER]\n";

    let main_code_builder =
        |sigact_addr: u32, pid_buf_addr: u32, main_marker_addr: u32| -> Vec<u8> {
            let mut out = Vec::with_capacity(96);
            // sys_rt_sigaction(SIGUSR1=10, &sigact, NULL, 8)
            out.extend_from_slice(&[0xB8, 0xAE, 0x00, 0x00, 0x00]); // mov eax, 174
            out.extend_from_slice(&[0xBB, 0x0A, 0x00, 0x00, 0x00]); // ebx = 10
            out.push(0xB9);
            out.extend_from_slice(&sigact_addr.to_le_bytes());
            out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (oldact NULL)
            out.extend_from_slice(&[0xBE, 0x08, 0x00, 0x00, 0x00]); // esi = 8 (sigsetsize)
            out.extend_from_slice(&[0xCD, 0x80]);
            // sys_getpid
            out.extend_from_slice(&[0xB8, 0x14, 0x00, 0x00, 0x00]); // mov eax, 20
            out.extend_from_slice(&[0xCD, 0x80]);
            out.push(0xA3); // ds:[pid_buf] = eax
            out.extend_from_slice(&pid_buf_addr.to_le_bytes());
            // sys_kill(pid, SIGUSR1=10)
            out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[pid_buf]
            out.extend_from_slice(&pid_buf_addr.to_le_bytes());
            out.extend_from_slice(&[0xB8, 0x25, 0x00, 0x00, 0x00]); // mov eax, 37
            out.extend_from_slice(&[0xB9, 0x0A, 0x00, 0x00, 0x00]); // mov ecx, 10
            out.extend_from_slice(&[0xCD, 0x80]);
            // After kill returns (handler ran + sigreturn'd, so main
            // resumes HERE): write(1, main_marker, len)
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&main_marker_addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&(main_marker.len() as u32).to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
            // exit(0)
            out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0x31, 0xDB]);
            out.extend_from_slice(&[0xCD, 0x80]);
            out
        };
    // Handler: write HANDLER marker, then `ret` (0xC3) — pops the
    // kernel-pushed pretcode (= sa_restorer) and runs the restorer.
    let handler_code_builder = |handler_marker_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&handler_marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(handler_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xC3); // ret -> restorer (via kernel-pushed pretcode)
        out
    };
    // Restorer: sys_rt_sigreturn (syscall 173).
    let restorer_code_builder = || -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&[0xB8, 0xAD, 0x00, 0x00, 0x00]); // mov eax, 173
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let main_len = main_code_builder(0, 0, 0).len() as u32;
    let handler_len = handler_code_builder(0).len() as u32;
    let restorer_len = restorer_code_builder().len() as u32;
    let main_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET;
    let handler_addr = main_addr + main_len;
    let restorer_addr = handler_addr + handler_len;
    let total_code_len = main_len + handler_len + restorer_len;
    let sigact_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + total_code_len;
    let pid_buf_addr = sigact_addr + 20;
    let main_marker_addr = pid_buf_addr + 4;
    let handler_marker_addr = main_marker_addr + main_marker.len() as u32;

    let main = main_code_builder(sigact_addr, pid_buf_addr, main_marker_addr);
    let handler = handler_code_builder(handler_marker_addr);
    let restorer = restorer_code_builder();
    let mut code = Vec::with_capacity(total_code_len as usize);
    code.extend_from_slice(&main);
    code.extend_from_slice(&handler);
    code.extend_from_slice(&restorer);

    let mut sigact_bytes = Vec::with_capacity(20);
    sigact_bytes.extend_from_slice(&handler_addr.to_le_bytes());
    // SA_RESTORER (0x0400_0000) | SA_SIGINFO (0x4). SA_SIGINFO is
    // essential: it makes the kernel build an *rt_sigframe* (so the
    // saved sigcontext lives inside uc.uc_mcontext) which is what our
    // restorer's `rt_sigreturn(173)` reads. Without it the kernel
    // builds a legacy `sigframe` (sigcontext directly after `sig`),
    // and rt_sigreturn reads uc_mcontext from the wrong offset →
    // restores a zero EIP/ESP → SIGSEGV. (That mismatch was the
    // entire "sigreturn segfault" blocker — a frame-type/restorer
    // mismatch in the test, not an emulator bug.)
    sigact_bytes.extend_from_slice(&0x04000004u32.to_le_bytes());
    sigact_bytes.extend_from_slice(&restorer_addr.to_le_bytes());
    sigact_bytes.extend_from_slice(&[0u8; 8]); // sa_mask

    let pid_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[&sigact_bytes, &pid_zeros, main_marker, handler_marker],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Signal /init that dumps the raw signal frame. The SIGUSR1
/// handler captures its entry ESP (= the rt_sigframe base the
/// kernel built) and `write`s 256 bytes of it to stdout between
/// `[USERSPACE FRAME=` and `]` markers, then `exit(0)`s (skipping
/// the broken sigreturn). The test decodes the frame to see what
/// the kernel actually wrote: `pretcode` at +0 (should equal the
/// restorer address), `sig` at +4 (should be 10), and — somewhere
/// in the saved sigcontext — the pre-signal EIP/ESP. If those are
/// zero, `setup_rt_frame`'s context save was dropped; if they hold
/// real values, the bug is on the `rt_sigreturn` *restore* side.
fn build_initramfs_signal_framedump() -> Vec<u8> {
    let frame_marker: &[u8] = b"[USERSPACE FRAME=";
    let post_marker: &[u8] = b"][USERSPACE END]\n";
    const DUMP_LEN: u32 = 256;

    let main_code_builder = |sigact_addr: u32, pid_buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        // sys_rt_sigaction(SIGUSR1=10, &sigact, NULL, 8)
        out.extend_from_slice(&[0xB8, 0xAE, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x0A, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&sigact_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xBE, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_getpid
        out.extend_from_slice(&[0xB8, 0x14, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&pid_buf_addr.to_le_bytes());
        // sys_kill(pid, SIGUSR1=10)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&pid_buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x25, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xB9, 0x0A, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // If we ever return here, just exit(0).
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };
    let handler_code_builder = |frame_marker_addr: u32, post_marker_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&[0x89, 0xE6]); // mov esi, esp (save frame base; survives int 0x80)
                                              // write(1, frame_marker, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&frame_marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(frame_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, esi, DUMP_LEN) — the raw frame
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x89, 0xF1]); // mov ecx, esi
        out.push(0xBA);
        out.extend_from_slice(&DUMP_LEN.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, post_marker, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&post_marker_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(post_marker.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let main_len = main_code_builder(0, 0).len() as u32;
    let handler_len = handler_code_builder(0, 0).len() as u32;
    let main_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET;
    let handler_addr = main_addr + main_len;
    // A restorer is still needed for SA_RESTORER, even though the
    // handler exits before using it.
    let restorer_addr = handler_addr + handler_len;
    let restorer = [0xB8u8, 0xAD, 0x00, 0x00, 0x00, 0xCD, 0x80]; // mov eax,173; int 0x80
    let total_code_len = main_len + handler_len + restorer.len() as u32;
    let sigact_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + total_code_len;
    let pid_buf_addr = sigact_addr + 20;
    let frame_marker_addr = pid_buf_addr + 4;
    let post_marker_addr = frame_marker_addr + frame_marker.len() as u32;

    let main = main_code_builder(sigact_addr, pid_buf_addr);
    let handler = handler_code_builder(frame_marker_addr, post_marker_addr);
    let mut code = Vec::with_capacity(total_code_len as usize);
    code.extend_from_slice(&main);
    code.extend_from_slice(&handler);
    code.extend_from_slice(&restorer);

    let mut sigact_bytes = Vec::with_capacity(20);
    sigact_bytes.extend_from_slice(&handler_addr.to_le_bytes());
    sigact_bytes.extend_from_slice(&0x04000000u32.to_le_bytes()); // SA_RESTORER
    sigact_bytes.extend_from_slice(&restorer_addr.to_le_bytes());
    sigact_bytes.extend_from_slice(&[0u8; 8]);
    let pid_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[&sigact_bytes, &pid_zeros, frame_marker, post_marker],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates a symlink `/link` → `/target`
/// via `sys_symlink("/target", "/link")` (syscall 83), then reads
/// it back via `sys_readlink("/link", buf, 32)` (syscall 85).
/// Emits the symlink return, the readlink return, and the first
/// 7 bytes of the readlink buffer between markers:
/// `[USERSPACE SYM=<4>RL=<4>LINK=<7>][USERSPACE END]`. Test
/// asserts `sym_ret == 0`, `rl_ret == 7`, and the buffer equals
/// `b"/target"`.
///
/// Pins:
///   - sys_symlink: kernel creates a symlink inode whose body
///     holds the target path string
///   - sys_readlink: kernel reads the symlink body back into a
///     user buffer (does NOT follow the link — returns the
///     target string itself, and the byte count as eax)
///   - the symlink-specific inode type (S_IFLNK) in tmpfs
fn build_initramfs_symlink() -> Vec<u8> {
    let target: &[u8] = b"/target\0";
    let link: &[u8] = b"/link\0";
    let marker_sym: &[u8] = b"[USERSPACE SYM=";
    let marker_rl: &[u8] = b"RL=";
    let marker_link: &[u8] = b"LINK=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |target_addr: u32,
                      link_addr: u32,
                      marker_sym_addr: u32,
                      marker_rl_addr: u32,
                      marker_link_addr: u32,
                      marker_post_addr: u32,
                      buf_addr: u32,
                      sym_ret_addr: u32,
                      rl_ret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // sys_symlink(target, link) — sys 83. ebx = target (oldname),
        // ecx = link (newname).
        out.extend_from_slice(&[0xB8, 0x53, 0x00, 0x00, 0x00]); // mov eax, 83
        out.push(0xBB);
        out.extend_from_slice(&target_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&link_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[sym_ret], eax
        out.extend_from_slice(&sym_ret_addr.to_le_bytes());
        // sys_readlink(link, buf, 32) — sys 85
        out.extend_from_slice(&[0xB8, 0x55, 0x00, 0x00, 0x00]); // mov eax, 85
        out.push(0xBB);
        out.extend_from_slice(&link_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x20, 0x00, 0x00, 0x00]); // mov edx, 32
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[rl_ret], eax
        out.extend_from_slice(&rl_ret_addr.to_le_bytes());
        // Helper to emit write(1, addr, len).
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        w(marker_sym_addr, marker_sym.len() as u32, &mut out);
        w(sym_ret_addr, 4, &mut out);
        w(marker_rl_addr, marker_rl.len() as u32, &mut out);
        w(rl_ret_addr, 4, &mut out);
        w(marker_link_addr, marker_link.len() as u32, &mut out);
        w(buf_addr, 7, &mut out); // first 7 bytes of readlink result
        w(marker_post_addr, marker_post.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let target_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let link_addr = target_addr + target.len() as u32;
    let marker_sym_addr = link_addr + link.len() as u32;
    let marker_rl_addr = marker_sym_addr + marker_sym.len() as u32;
    let marker_link_addr = marker_rl_addr + marker_rl.len() as u32;
    let marker_post_addr = marker_link_addr + marker_link.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    let sym_ret_addr = buf_addr + 32;
    let rl_ret_addr = sym_ret_addr + 4;
    let code = build_code(
        target_addr,
        link_addr,
        marker_sym_addr,
        marker_rl_addr,
        marker_link_addr,
        marker_post_addr,
        buf_addr,
        sym_ret_addr,
        rl_ret_addr,
    );
    let buf_zeros = [0u8; 32];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            target,
            link,
            marker_sym,
            marker_rl,
            marker_link,
            marker_post,
            &buf_zeros,
            &ret_zeros,
            &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates `/a` with "HARDLINK", hard-
/// links it to `/b` via `sys_link("/a", "/b")` (syscall 9), then
/// opens `/b` and reads the content back. Emits the link return
/// and the 8-byte readback between
/// `[USERSPACE LINK_RET=<4>DATA=<8>][USERSPACE END]`. Test asserts
/// `link_ret == 0` and the content equals `b"HARDLINK"`.
///
/// Distinct from the symlink milestone: a hard link is a SECOND
/// dentry pointing at the SAME inode (i_nlink incremented), not a
/// separate symlink inode. Reading through `/b` returns `/a`'s
/// content because they share one inode + one set of data blocks.
fn build_initramfs_hardlink() -> Vec<u8> {
    let path_a: &[u8] = b"/a\0";
    let path_b: &[u8] = b"/b\0";
    let payload: &[u8] = b"HARDLINK"; // 8 bytes
    let marker_pre: &[u8] = b"[USERSPACE LINK_RET=";
    let marker_mid: &[u8] = b"DATA=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_a_addr: u32,
                      path_b_addr: u32,
                      payload_addr: u32,
                      marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      ret_buf_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // fd = sys_open("/a", O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_a_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 8)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_link("/a", "/b") — sys 9. ebx=oldname, ecx=newname.
        out.extend_from_slice(&[0xB8, 0x09, 0x00, 0x00, 0x00]); // mov eax, 9
        out.push(0xBB);
        out.extend_from_slice(&path_a_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&path_b_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[ret_buf], eax
        out.extend_from_slice(&ret_buf_addr.to_le_bytes());
        // fd = sys_open("/b", O_RDONLY, 0)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_b_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_read(fd, buf, 8)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // Emit: marker_pre, ret_buf(4), marker_mid, buf(8), marker_post.
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        w(marker_pre_addr, marker_pre.len() as u32, &mut out);
        w(ret_buf_addr, 4, &mut out);
        w(marker_mid_addr, marker_mid.len() as u32, &mut out);
        w(buf_addr, 8, &mut out);
        w(marker_post_addr, marker_post.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let path_a_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let path_b_addr = path_a_addr + path_a.len() as u32;
    let payload_addr = path_b_addr + path_b.len() as u32;
    let marker_pre_addr = payload_addr + payload.len() as u32;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_post_addr = marker_mid_addr + marker_mid.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let ret_buf_addr = fd_addr + 4;
    let buf_addr = ret_buf_addr + 4;
    let code = build_code(
        path_a_addr,
        path_b_addr,
        payload_addr,
        marker_pre_addr,
        marker_mid_addr,
        marker_post_addr,
        fd_addr,
        ret_buf_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let ret_zeros = [0u8; 4];
    let buf_zeros = [0u8; 8];
    let binary = make_init_elf32_safe(
        &code,
        &[
            path_a,
            path_b,
            payload,
            marker_pre,
            marker_mid,
            marker_post,
            &fd_zeros,
            &ret_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init enumerates a directory:
///
///   sys_mkdir("/d", 0o755)
///   fd = open("/d/ZZMARKER", O_CREAT|O_WRONLY, 0o644); close(fd)
///   dirfd = open("/d", O_RDONLY|O_DIRECTORY)
///   n = sys_getdents64(dirfd, buf, 512)   ; syscall 220
///   close(dirfd)
///   write `[USERSPACE DENTS=<n:4>BUF=` + buf[0..n] + `][USERSPACE END]\n`
///
/// The kernel fills `buf` with `struct linux_dirent64` records
/// (`{u64 d_ino, s64 d_off, u16 d_reclen, u8 d_type, char d_name[]}`),
/// one per entry — ".", "..", and "ZZMARKER". The test asserts
/// `n > 0` and that the byte string `b"ZZMARKER"` appears in the
/// dumped buffer (the NUL-terminated d_name of our created file).
///
/// Pins directory enumeration end-to-end: the kernel walks the
/// dir inode's children and serializes their dirent records into
/// a user buffer — the primitive every `ls`/`readdir` needs.
fn build_initramfs_getdents() -> Vec<u8> {
    let dir_path: &[u8] = b"/d\0";
    let file_path: &[u8] = b"/d/ZZMARKER\0";
    let marker_pre: &[u8] = b"[USERSPACE DENTS=";
    let marker_mid: &[u8] = b"BUF=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |dir_path_addr: u32,
                      file_path_addr: u32,
                      marker_pre_addr: u32,
                      marker_mid_addr: u32,
                      marker_post_addr: u32,
                      fd_addr: u32,
                      n_buf_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(320);
        // sys_mkdir("/d", 0o755) — sys 39
        out.extend_from_slice(&[0xB8, 0x27, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0xED, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // fd = open("/d/ZZMARKER", O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&file_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // dirfd = open("/d", O_RDONLY|O_DIRECTORY=0x10000)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x00, 0x00, 0x01, 0x00]); // mov ecx, 0x10000
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[fd] = dirfd
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // n = sys_getdents64(dirfd, buf, 512) — sys 220
        out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd]
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0xDC, 0x00, 0x00, 0x00]); // mov eax, 220
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x00, 0x02, 0x00, 0x00]); // mov edx, 512
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[n_buf] = eax (byte count)
        out.extend_from_slice(&n_buf_addr.to_le_bytes());
        // close(dirfd)
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
        // write(1, n_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&n_buf_addr.to_le_bytes());
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
        // write(1, buf, n)  — edx = ds:[n_buf]
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0x8B, 0x15]); // mov edx, ds:[n_buf]
        out.extend_from_slice(&n_buf_addr.to_le_bytes());
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

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let dir_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let file_path_addr = dir_path_addr + dir_path.len() as u32;
    let marker_pre_addr = file_path_addr + file_path.len() as u32;
    let marker_mid_addr = marker_pre_addr + marker_pre.len() as u32;
    let marker_post_addr = marker_mid_addr + marker_mid.len() as u32;
    let fd_addr = marker_post_addr + marker_post.len() as u32;
    let n_buf_addr = fd_addr + 4;
    let buf_addr = n_buf_addr + 4;
    let code = build_code(
        dir_path_addr,
        file_path_addr,
        marker_pre_addr,
        marker_mid_addr,
        marker_post_addr,
        fd_addr,
        n_buf_addr,
        buf_addr,
    );
    let fd_zeros = [0u8; 4];
    let n_zeros = [0u8; 4];
    let buf_zeros = [0u8; 512];
    let binary = make_init_elf32_safe(
        &code,
        &[
            dir_path,
            file_path,
            marker_pre,
            marker_mid,
            marker_post,
            &fd_zeros,
            &n_zeros,
            &buf_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init calls `sys_statfs("/", &buf)`
/// (syscall 99) and reads `f_type` (first 4 bytes of struct
/// statfs) back out. Writes the 4-byte magic between
/// `[USERSPACE FS_TYPE=…][USERSPACE END]`. Test asserts the
/// value equals `TMPFS_MAGIC = 0x01021994` — proves the rootfs
/// is tmpfs as expected, AND that the kernel fills the statfs
/// struct correctly via `copy_to_user`.
fn build_initramfs_statfs() -> Vec<u8> {
    let path: &[u8] = b"/\0";
    let marker_pre: &[u8] = b"[USERSPACE FS_TYPE=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |path_addr: u32,
                      marker_pre_addr: u32,
                      marker_post_addr: u32,
                      buf_addr: u32,
                      type_buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // sys_statfs64(path, size=84, &buf) — sys 268.
        // Modern kernels prefer this over legacy sys_statfs;
        // first attempt with syscall 99 returned 0 in f_type,
        // suggesting it may have returned -errno in our build.
        out.extend_from_slice(&[0xB8, 0x0C, 0x01, 0x00, 0x00]); // mov eax, 268
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x54, 0x00, 0x00, 0x00]); // mov ecx, 84
        out.push(0xBA);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // mov eax, ds:[buf+0] (f_type) → ds:[type_buf]
        out.push(0xA1);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.push(0xA3);
        out.extend_from_slice(&type_buf_addr.to_le_bytes());
        // write(1, marker_pre, len)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&marker_pre_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(marker_pre.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(1, type_buf, 4)
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&type_buf_addr.to_le_bytes());
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

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_pre_addr = path_addr + path.len() as u32;
    let marker_post_addr = marker_pre_addr + marker_pre.len() as u32;
    let buf_addr = marker_post_addr + marker_post.len() as u32;
    // struct statfs64 is 84 bytes on i386
    let type_buf_addr = buf_addr + 84;
    let code = build_code(
        path_addr,
        marker_pre_addr,
        marker_post_addr,
        buf_addr,
        type_buf_addr,
    );
    let buf_zeros = [0u8; 84];
    let type_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[path, marker_pre, marker_post, &buf_zeros, &type_zeros],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init forks; the child calls
/// `sys_getppid` (syscall 64) and writes the returned PID
/// between `[USERSPACE PARENT_PID=…][USERSPACE END]`. Parent
/// waitpid's the child. Test asserts the child's getppid result
/// equals 1 — proves the kernel set the child's `real_parent`
/// to point at /init (PID 1) on fork.
fn build_initramfs_getppid_in_child() -> Vec<u8> {
    let marker_pre: &[u8] = b"[USERSPACE PARENT_PID=";
    let marker_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |marker_pre_addr: u32, marker_post_addr: u32, buf_addr: u32| -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        // sys_fork
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]); // mov eax, 2
        out.extend_from_slice(&[0xCD, 0x80]);
        // test eax, eax; jnz parent (disp8 patched after we know
        // the child block's size)
        out.extend_from_slice(&[0x85, 0xC0]);
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]); // placeholder

        // child:
        // sys_getppid → eax
        out.extend_from_slice(&[0xB8, 0x40, 0x00, 0x00, 0x00]); // mov eax, 64
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[buf] = eax
        out.extend_from_slice(&buf_addr.to_le_bytes());
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
        // child exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);

        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;

        // parent: sys_waitpid(-1, NULL, 0); exit(0)
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]); // mov eax, 7
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
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
    let binary = make_init_elf32_safe(&code, &[marker_pre, marker_post, &buf_zeros], 7);
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init creates `/probe` with 10 bytes,
/// calls `sys_truncate("/probe", 4)` (syscall 92), then
/// `sys_stat64` and reads `st_size` (low 4 bytes at offset 44)
/// back out. Writes the 4-byte size between
/// `[USERSPACE TRUNC_SIZE=…][USERSPACE END]`. Test asserts the
/// value equals 4 — proves the kernel shrank the file via
/// truncate, not just left it at the original 10 bytes.
///
/// Different from the stat milestone: that one only proves
/// write extends `i_size`. This one proves truncate SHRINKS it
/// (kernel calls `setattr_should_drop_sgid` and
/// `vmtruncate` → `simple_setsize` to shrink the page cache and
/// inode's `i_size`).
fn build_initramfs_truncate() -> Vec<u8> {
    let path: &[u8] = b"/probe\0";
    let payload: &[u8] = b"ABCDEFGHIJ"; // 10 bytes
    let marker_pre: &[u8] = b"[USERSPACE TRUNC_SIZE=";
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
        // fd = sys_open(path, O_CREAT|O_WRONLY, 0o644)
        out.extend_from_slice(&[0xB8, 0x05, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x41, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBA, 0xA4, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        // sys_write(fd, payload, 10)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&payload_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x0A, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_close(fd)
        out.extend_from_slice(&[0x8B, 0x1D]);
        out.extend_from_slice(&fd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_truncate(path, 4) — sys 92
        out.extend_from_slice(&[0xB8, 0x5C, 0x00, 0x00, 0x00]); // mov eax, 92
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x04, 0x00, 0x00, 0x00]); // mov ecx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        // sys_stat64(path, &statbuf) — sys 195
        out.extend_from_slice(&[0xB8, 0xC3, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&path_addr.to_le_bytes());
        out.push(0xB9);
        out.extend_from_slice(&statbuf_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // Read st_size low 4 bytes at offset 44 and store.
        out.push(0xA1);
        out.extend_from_slice(&(statbuf_addr + 44).to_le_bytes());
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
    let size_buf_addr = statbuf_addr + 100;
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

/// Build a cpio whose /init calls `sys_mkdir("/sub", 0o755)`,
/// `sys_chdir("/sub")`, then `sys_getcwd(buf, 32)`, capturing all
/// three syscall returns plus the cwd buffer between markers
/// `[USERSPACE MKDIR=<4>CHDIR=<4>GETCWD=<4>PWD=<8>][USERSPACE END]`.
/// Test asserts mkdir==0, chdir==0, getcwd==5 (len "/sub\0"), and
/// cwd starts with "/sub". Pins:
///   - sys_chdir: kernel updates the current task's `fs->pwd`
///     to point at the new dentry
///   - sys_getcwd: kernel walks the dentry tree from the
///     current `fs->pwd` up to its mount root and assembles a
///     pathname string into the user buffer (different mechanism
///     than read/stat — a kernel-side string build + copy_to_user)
///
/// The `PWD=` marker is deliberately NOT a substring of `GETCWD=`
/// — an earlier "CWD=" choice collided with the GETCWD= label and
/// produced a spurious zero reading that masqueraded as a getcwd
/// kernel bug. See the milestone test for the full history.
fn build_initramfs_chdir() -> Vec<u8> {
    let dir_path: &[u8] = b"/sub\0";
    let m_mkdir: &[u8] = b"[USERSPACE MKDIR=";
    let m_chdir: &[u8] = b"CHDIR=";
    let m_getcwd: &[u8] = b"GETCWD=";
    // NOTE: must NOT be a substring of m_getcwd ("GETCWD=" contains
    // "CWD="!), or the test's position() search collides. Use "PWD=".
    let m_cwd: &[u8] = b"PWD=";
    let m_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |dir_path_addr: u32,
                      m_mkdir_addr: u32,
                      m_chdir_addr: u32,
                      m_getcwd_addr: u32,
                      m_cwd_addr: u32,
                      m_post_addr: u32,
                      buf_addr: u32,
                      mkdir_ret_addr: u32,
                      chdir_ret_addr: u32,
                      getcwd_ret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        // sys_mkdir(dir_path, 0o755) — sys 39
        out.extend_from_slice(&[0xB8, 0x27, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0xED, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[mkdir_ret] = eax
        out.extend_from_slice(&mkdir_ret_addr.to_le_bytes());
        // sys_chdir(dir_path) — sys 12
        out.extend_from_slice(&[0xB8, 0x0C, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&dir_path_addr.to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[chdir_ret] = eax
        out.extend_from_slice(&chdir_ret_addr.to_le_bytes());
        // sys_getcwd(buf, 32) — sys 183
        out.extend_from_slice(&[0xB8, 0xB7, 0x00, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x20, 0x00, 0x00, 0x00]); // mov ecx, 32
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[getcwd_ret] = eax
        out.extend_from_slice(&getcwd_ret_addr.to_le_bytes());
        // Emit all four labelled values + 8 cwd bytes.
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        w(m_mkdir_addr, m_mkdir.len() as u32, &mut out);
        w(mkdir_ret_addr, 4, &mut out);
        w(m_chdir_addr, m_chdir.len() as u32, &mut out);
        w(chdir_ret_addr, 4, &mut out);
        w(m_getcwd_addr, m_getcwd.len() as u32, &mut out);
        w(getcwd_ret_addr, 4, &mut out);
        w(m_cwd_addr, m_cwd.len() as u32, &mut out);
        w(buf_addr, 8, &mut out);
        w(m_post_addr, m_post.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let dir_path_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_mkdir_addr = dir_path_addr + dir_path.len() as u32;
    let m_chdir_addr = m_mkdir_addr + m_mkdir.len() as u32;
    let m_getcwd_addr = m_chdir_addr + m_chdir.len() as u32;
    let m_cwd_addr = m_getcwd_addr + m_getcwd.len() as u32;
    let m_post_addr = m_cwd_addr + m_cwd.len() as u32;
    let buf_addr = m_post_addr + m_post.len() as u32;
    let mkdir_ret_addr = buf_addr + 32;
    let chdir_ret_addr = mkdir_ret_addr + 4;
    let getcwd_ret_addr = chdir_ret_addr + 4;
    let code = build_code(
        dir_path_addr,
        m_mkdir_addr,
        m_chdir_addr,
        m_getcwd_addr,
        m_cwd_addr,
        m_post_addr,
        buf_addr,
        mkdir_ret_addr,
        chdir_ret_addr,
        getcwd_ret_addr,
    );
    let buf_zeros = [0u8; 32];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            dir_path, m_mkdir, m_chdir, m_getcwd, m_cwd, m_post, &buf_zeros, &ret_zeros,
            &ret_zeros, &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init does a full pipe round-trip and
/// reports EVERY syscall return, emitting the diagnostics that
/// precede the (potentially blocking) read BEFORE the read, so a
/// blocked read still leaves the decisive data in UART:
///
///   pipe2(&fds, 0)                          -> p2_ret; fds=[r,w]
///   wr = write(fds[1], "PIPE", 4)
///   write(1, "[USERSPACE P2=<p2>F0=<r>F1=<w>WR=<wr> ", ...)  <-- before read
///   rd = read(fds[0], buf, 4)               <-- may block if pipe empty
///   write(1, "RD=<rd>BUF=<buf>][USERSPACE END]\n", ...)
///
/// All fd/return values are read from the fds buffer / stashed
/// eax. Output via fd 1 (the console; the pipe write end is fd 4
/// per the diag, distinct from fd 1, so this is safe). Test uses
/// a bounded step budget so a blocked read fails fast.
fn build_initramfs_pipe_rt() -> Vec<u8> {
    let pipe_msg: &[u8] = b"PIPE";
    let m_p2: &[u8] = b"[USERSPACE P2=";
    let m_f0: &[u8] = b"F0=";
    let m_f1: &[u8] = b"F1=";
    let m_wr: &[u8] = b"WR=";
    let m_rd: &[u8] = b"RD=";
    let m_buf: &[u8] = b"BUF=";
    let m_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |pipe_msg_addr: u32,
                      m_p2_addr: u32,
                      m_f0_addr: u32,
                      m_f1_addr: u32,
                      m_wr_addr: u32,
                      m_rd_addr: u32,
                      m_buf_addr: u32,
                      m_post_addr: u32,
                      fds_addr: u32,
                      buf_addr: u32,
                      p2_ret_addr: u32,
                      wr_ret_addr: u32,
                      rd_ret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(384);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // sys_pipe2(&fds, 0) — sys 331
        out.extend_from_slice(&[0xB8, 0x4B, 0x01, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[p2_ret] = eax
        out.extend_from_slice(&p2_ret_addr.to_le_bytes());
        // Emit fds IMMEDIATELY after pipe2 (before any write), so
        // F0/F1 reflect exactly what pipe2 populated, isolated from
        // any later effect. Before the REP-string #PF rollback fix
        // (commit 253e751) these read [0,0] because pipe2's
        // copy_to_user landed nothing on the fresh fds page; with the
        // fix they hold the real fd pair (read end ≥3, write end =
        // read+1) — see linux_userspace_pipe_milestone for the guard.
        w(m_p2_addr, m_p2.len() as u32, &mut out);
        w(p2_ret_addr, 4, &mut out);
        w(m_f0_addr, m_f0.len() as u32, &mut out);
        w(fds_addr, 4, &mut out); // fds[0] immediately post-pipe2
        w(m_f1_addr, m_f1.len() as u32, &mut out);
        w(fds_addr + 4, 4, &mut out); // fds[1] immediately post-pipe2
                                      // wr = write(fds[1], pipe_msg, 4)
        out.push(0xA1); // mov eax, ds:[fds+4]  (write end)
        out.extend_from_slice(&(fds_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&pipe_msg_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]); // mov edx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[wr_ret] = eax
        out.extend_from_slice(&wr_ret_addr.to_le_bytes());
        // Emit WR (the immediate F0/F1 + P2 were emitted above).
        w(m_wr_addr, m_wr.len() as u32, &mut out);
        w(wr_ret_addr, 4, &mut out);
        // rd = read(fds[0], buf, 4)  — may block if pipe empty
        out.push(0xA1); // mov eax, ds:[fds]  (read end)
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]); // mov edx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // ds:[rd_ret] = eax
        out.extend_from_slice(&rd_ret_addr.to_le_bytes());
        // Emit post-read: RD, BUF, end.
        w(m_rd_addr, m_rd.len() as u32, &mut out);
        w(rd_ret_addr, 4, &mut out);
        w(m_buf_addr, m_buf.len() as u32, &mut out);
        w(buf_addr, 4, &mut out);
        w(m_post_addr, m_post.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0).len() as u32;
    let pipe_msg_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_p2_addr = pipe_msg_addr + pipe_msg.len() as u32;
    let m_f0_addr = m_p2_addr + m_p2.len() as u32;
    let m_f1_addr = m_f0_addr + m_f0.len() as u32;
    let m_wr_addr = m_f1_addr + m_f1.len() as u32;
    let m_rd_addr = m_wr_addr + m_wr.len() as u32;
    let m_buf_addr = m_rd_addr + m_rd.len() as u32;
    let m_post_addr = m_buf_addr + m_buf.len() as u32;
    let fds_addr = m_post_addr + m_post.len() as u32;
    let buf_addr = fds_addr + 8;
    let p2_ret_addr = buf_addr + 4;
    let wr_ret_addr = p2_ret_addr + 4;
    let rd_ret_addr = wr_ret_addr + 4;
    let code = build_code(
        pipe_msg_addr,
        m_p2_addr,
        m_f0_addr,
        m_f1_addr,
        m_wr_addr,
        m_rd_addr,
        m_buf_addr,
        m_post_addr,
        fds_addr,
        buf_addr,
        p2_ret_addr,
        wr_ret_addr,
        rd_ret_addr,
    );
    let fds_zeros = [0u8; 8];
    let buf_zeros = [0u8; 4];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            pipe_msg, m_p2, m_f0, m_f1, m_wr, m_rd, m_buf, m_post, &fds_zeros, &buf_zeros,
            &ret_zeros, &ret_zeros, &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// /init that exercises the stdin read path: emits a "blocking on
/// read" marker, calls `read(0, buf, 4)` (which blocks until the
/// host injects input via `send_input`), then emits the read's
/// return value and the bytes that landed in `buf`. This is the
/// symmetric counterpart to the write path and the regression
/// probe for the REP-string #PF rollback fix on the *delivery*
/// side: `read`'s `copy_to_user` writes into `buf`, which sits on
/// a fresh demand-zero page, so the first write faults exactly
/// like `pipe2`'s fd-pair copy did.
fn build_initramfs_read_stdin() -> Vec<u8> {
    let m_q: &[u8] = b"[USERSPACE Q]\n"; // "blocking on read now — send input"
    let m_rc: &[u8] = b"RC=";
    let m_got: &[u8] = b"GOT=";
    let m_post: &[u8] = b"][USERSPACE END]\n";

    let build_code = |m_q_addr: u32,
                      m_rc_addr: u32,
                      m_got_addr: u32,
                      m_post_addr: u32,
                      buf_addr: u32,
                      rd_ret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9); // mov ecx, addr
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA); // mov edx, len
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // write(1, m_q, len) — signal we're about to block in read
        w(m_q_addr, m_q.len() as u32, &mut out);
        // read(0, buf, 4) — sys 3, fd 0, buf, 4 bytes; blocks until
        // the host send_input()s a line.
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3 (read)
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (fd 0)
        out.push(0xB9); // mov ecx, buf_addr
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x04, 0x00, 0x00, 0x00]); // mov edx, 4
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[rd_ret], eax
        out.extend_from_slice(&rd_ret_addr.to_le_bytes());
        // RC=<rd_ret>
        w(m_rc_addr, m_rc.len() as u32, &mut out);
        w(rd_ret_addr, 4, &mut out);
        // GOT=<buf>
        w(m_got_addr, m_got.len() as u32, &mut out);
        w(buf_addr, 4, &mut out);
        // END
        w(m_post_addr, m_post.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0).len() as u32;
    let m_q_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_rc_addr = m_q_addr + m_q.len() as u32;
    let m_got_addr = m_rc_addr + m_rc.len() as u32;
    let m_post_addr = m_got_addr + m_got.len() as u32;
    let buf_addr = m_post_addr + m_post.len() as u32;
    let rd_ret_addr = buf_addr + 4;
    let code = build_code(
        m_q_addr,
        m_rc_addr,
        m_got_addr,
        m_post_addr,
        buf_addr,
        rd_ret_addr,
    );
    let buf_zeros = [0u8; 4];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[m_q, m_rc, m_got, m_post, &buf_zeros, &ret_zeros],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init exercises `sys_poll` on a pipe: it makes
/// a pipe, writes one byte into it (so the read end becomes
/// readable), then `poll([{fd: read_end, events: POLLIN}], 1,
/// timeout=0)` and emits the poll return plus the raw 8-byte
/// `pollfd` struct:
/// `[USERSPACE POLL=<4>PFD=<8>][USERSPACE END]`.
///
/// Pins the readiness-notification path:
///   * `poll` `copy_from_user`s the pollfd array (reads `events`),
///   * the kernel's pipe `->poll` reports POLLIN because a byte is
///     buffered,
///   * `poll` `copy_to_user`s `revents` back into the (fresh) user
///     struct and returns the count of ready fds.
///
/// The test asserts `poll` returns 1 and `revents & POLLIN` is set.
fn build_initramfs_poll_pipe() -> Vec<u8> {
    let msg: &[u8] = b"X";
    let m_poll: &[u8] = b"[USERSPACE POLL=";
    let m_pfd: &[u8] = b"PFD=";
    let m_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |m_poll_addr: u32,
                      m_pfd_addr: u32,
                      m_end_addr: u32,
                      msg_addr: u32,
                      fds_addr: u32,
                      pollfd_addr: u32,
                      pollret_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // pipe2(&fds, 0) — sys 331
        out.extend_from_slice(&[0xB8, 0x4B, 0x01, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0xCD, 0x80]);
        // write(fds[1], msg, 1) — make the read end readable
        out.push(0xA1); // mov eax, ds:[fds+4]
        out.extend_from_slice(&(fds_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1
        out.extend_from_slice(&[0xCD, 0x80]);
        // pollfd.fd = fds[0]
        out.push(0xA1); // mov eax, ds:[fds]
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.push(0xA3); // mov ds:[pollfd], eax
        out.extend_from_slice(&pollfd_addr.to_le_bytes());
        // pollfd.events = POLLIN(1), pollfd.revents = 0
        // (one 32-bit store: low16 = events, high16 = revents).
        out.extend_from_slice(&[0xC7, 0x05]); // mov dword ds:[pollfd+4], imm32
        out.extend_from_slice(&(pollfd_addr + 4).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        // poll(&pollfd, 1, 0) — sys 168
        out.extend_from_slice(&[0xB8, 0xA8, 0x00, 0x00, 0x00]); // mov eax, 168
        out.push(0xBB);
        out.extend_from_slice(&pollfd_addr.to_le_bytes());
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, 1 (nfds)
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (timeout=0)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[pollret], eax
        out.extend_from_slice(&pollret_addr.to_le_bytes());
        // emit POLL=<ret> PFD=<8-byte struct> END
        w(m_poll_addr, m_poll.len() as u32, &mut out);
        w(pollret_addr, 4, &mut out);
        w(m_pfd_addr, m_pfd.len() as u32, &mut out);
        w(pollfd_addr, 8, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0, 0, 0).len() as u32;
    let m_poll_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_pfd_addr = m_poll_addr + m_poll.len() as u32;
    let m_end_addr = m_pfd_addr + m_pfd.len() as u32;
    let msg_addr = m_end_addr + m_end.len() as u32;
    let fds_addr = msg_addr + msg.len() as u32;
    let pollfd_addr = fds_addr + 8;
    let pollret_addr = pollfd_addr + 8;
    let code = build_code(
        m_poll_addr,
        m_pfd_addr,
        m_end_addr,
        msg_addr,
        fds_addr,
        pollfd_addr,
        pollret_addr,
    );
    let fds_zeros = [0u8; 8];
    let pollfd_zeros = [0u8; 8];
    let ret_zeros = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[
            m_poll,
            m_pfd,
            m_end,
            msg,
            &fds_zeros,
            &pollfd_zeros,
            &ret_zeros,
        ],
        7,
    );
    build_cpio_archive(&binary, /* proc_dir */ false)
}

/// Build a cpio whose /init runs a real `cmd1 | cmd2` shell
/// pipeline: it makes a pipe, forks a *writer* child, and the
/// *parent* is the *reader*.
///
///   * writer child: `dup2(pipe_write, 1)`, closes both original
///     pipe fds, `write(1, "PIPED", 5)` (which now lands in the
///     pipe), `exit(0)`,
///   * parent reader: closes the write end, `dup2(pipe_read, 0)`,
///     closes the original read fd, `read(0, buf, 5)` (which now
///     reads from the pipe), reaps the child with `waitpid`, then
///     reports what it received to the real console:
///     `[USERSPACE PIPELINE=PIPED][USERSPACE END]`.
///
/// This is the capstone integration of every primitive proven
/// separately — pipe, fork, dup2, close, blocking read/write across
/// processes, and waitpid — and is exactly the fd-plumbing a shell
/// performs for `echo PIPED | cat`. The test asserts the parent
/// read back `"PIPED"`, i.e. bytes flowed writer-stdout → pipe →
/// reader-stdin across the process boundary.
fn build_initramfs_pipeline() -> Vec<u8> {
    let msg: &[u8] = b"PIPED";
    let m_pipeline: &[u8] = b"[USERSPACE PIPELINE=";
    let m_end: &[u8] = b"][USERSPACE END]\n";

    let build_code = |m_pipeline_addr: u32,
                      m_end_addr: u32,
                      msg_addr: u32,
                      fds_addr: u32,
                      buf_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        let close = |fd_mem: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x06, 0x00, 0x00, 0x00]); // mov eax, 6 (close)
            out.extend_from_slice(&[0x8B, 0x1D]); // mov ebx, ds:[fd_mem]
            out.extend_from_slice(&fd_mem.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // pipe2(&fds, 0) — sys 331
        out.extend_from_slice(&[0xB8, 0x4B, 0x01, 0x00, 0x00]);
        out.push(0xBB);
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
        out.extend_from_slice(&[0xCD, 0x80]);
        // fork — sys 2
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched)

        // ===== CHILD (writer): eax == 0 =====
        // dup2(fds[1], 1): ebx=oldfd=fds[1], ecx=newfd=1
        out.push(0xA1); // mov eax, ds:[fds+4]
        out.extend_from_slice(&(fds_addr + 4).to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0xB9, 0x01, 0x00, 0x00, 0x00]); // mov ecx, 1
        out.extend_from_slice(&[0xB8, 0x3F, 0x00, 0x00, 0x00]); // mov eax, 63 (dup2)
        out.extend_from_slice(&[0xCD, 0x80]);
        close(fds_addr, &mut out); // close(fds[0])
        close(fds_addr + 4, &mut out); // close(fds[1])
                                       // write(1, msg, 5) — fd 1 is now the pipe write end
        out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]);
        out.push(0xB9);
        out.extend_from_slice(&msg_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(msg.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);

        // ===== PARENT (reader) =====
        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;
        close(fds_addr + 4, &mut out); // close(fds[1]) — parent drops write end
                                       // dup2(fds[0], 0): ebx=fds[0], ecx=0
        out.push(0xA1); // mov eax, ds:[fds]
        out.extend_from_slice(&fds_addr.to_le_bytes());
        out.extend_from_slice(&[0x89, 0xC3]); // mov ebx, eax
        out.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx (newfd = 0)
        out.extend_from_slice(&[0xB8, 0x3F, 0x00, 0x00, 0x00]); // mov eax, 63 (dup2)
        out.extend_from_slice(&[0xCD, 0x80]);
        close(fds_addr, &mut out); // close(fds[0]) — original read fd
                                   // read(0, buf, 5) — stdin is now the pipe read end
        out.extend_from_slice(&[0xB8, 0x03, 0x00, 0x00, 0x00]); // mov eax, 3 (read)
        out.extend_from_slice(&[0x31, 0xDB]); // xor ebx, ebx (fd 0)
        out.push(0xB9);
        out.extend_from_slice(&buf_addr.to_le_bytes());
        out.push(0xBA);
        out.extend_from_slice(&(msg.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xCD, 0x80]);
        // waitpid(-1, NULL, 0) — reap the writer
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]);
        out.extend_from_slice(&[0x31, 0xC9]);
        out.extend_from_slice(&[0x31, 0xD2]);
        out.extend_from_slice(&[0xCD, 0x80]);
        // emit PIPELINE=<buf> END to the console (parent's fd 1)
        w(m_pipeline_addr, m_pipeline.len() as u32, &mut out);
        w(buf_addr, msg.len() as u32, &mut out);
        w(m_end_addr, m_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let m_pipeline_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let m_end_addr = m_pipeline_addr + m_pipeline.len() as u32;
    let msg_addr = m_end_addr + m_end.len() as u32;
    let fds_addr = msg_addr + msg.len() as u32;
    let buf_addr = fds_addr + 8;
    let code = build_code(m_pipeline_addr, m_end_addr, msg_addr, fds_addr, buf_addr);
    let fds_zeros = [0u8; 8];
    let buf_zeros = [0u8; 8];
    let binary = make_init_elf32_safe(&code, &[m_pipeline, m_end, msg, &fds_zeros, &buf_zeros], 7);
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

/// Build a cpio whose /init forks a child that `exit(42)`s, then
/// reaps it with `waitpid(-1, &status, 0)` and reports both the
/// raw status word and waitpid's return (the child PID):
/// `[USERSPACE WSTATUS=<4>WPID=<4>][USERSPACE END]`.
///
/// Pins child reaping end-to-end — beyond the fork milestone (which
/// waits with `status=NULL` purely for ordering), this captures the
/// child's exit status:
///   * the kernel encodes a normal exit as `status = exit_code << 8`
///     (so `WEXITSTATUS == 42`, `WIFEXITED` true),
///   * `waitpid` `copy_to_user`s that word into a fresh user buffer,
///   * `waitpid` returns the reaped child's PID.
///
/// The test asserts `WEXITSTATUS(status) == 42`, the WIFEXITED low
/// bits are zero, and the returned PID is a real child (`> 1`).
fn build_initramfs_wait_status() -> Vec<u8> {
    let marker_ws: &[u8] = b"[USERSPACE WSTATUS=";
    let marker_wpid: &[u8] = b"WPID=";
    let marker_end: &[u8] = b"][USERSPACE END]\n";
    const CHILD_EXIT_CODE: u8 = 42;

    let build_code = |marker_ws_addr: u32,
                      marker_wpid_addr: u32,
                      marker_end_addr: u32,
                      status_addr: u32,
                      wpid_addr: u32|
     -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        let w = |addr: u32, len: u32, out: &mut Vec<u8>| {
            out.extend_from_slice(&[0xB8, 0x04, 0x00, 0x00, 0x00]); // mov eax, 4 (write)
            out.extend_from_slice(&[0xBB, 0x01, 0x00, 0x00, 0x00]); // mov ebx, 1 (stdout)
            out.push(0xB9);
            out.extend_from_slice(&addr.to_le_bytes());
            out.push(0xBA);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0xCD, 0x80]);
        };
        // sys_fork — sys 2
        out.extend_from_slice(&[0xB8, 0x02, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        let jnz_at = out.len();
        out.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched below)
                                              // child: exit(CHILD_EXIT_CODE)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
        out.extend_from_slice(&[0xBB, CHILD_EXIT_CODE, 0x00, 0x00, 0x00]); // mov ebx, 42
        out.extend_from_slice(&[0xCD, 0x80]);
        // parent:
        let parent_start = out.len();
        let disp = (parent_start - (jnz_at + 2)) as i32;
        assert!(
            (-128..=127).contains(&disp),
            "child block too large for jnz disp8 (got {disp})"
        );
        out[jnz_at + 1] = disp as u8;
        // sys_waitpid(-1, &status, 0) — sys 7
        out.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]); // mov eax, 7
        out.extend_from_slice(&[0xBB, 0xFF, 0xFF, 0xFF, 0xFF]); // mov ebx, -1
        out.push(0xB9); // mov ecx, &status
        out.extend_from_slice(&status_addr.to_le_bytes());
        out.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx (options=0)
        out.extend_from_slice(&[0xCD, 0x80]);
        out.push(0xA3); // mov ds:[wpid], eax (waitpid return = child pid)
        out.extend_from_slice(&wpid_addr.to_le_bytes());
        // emit WSTATUS=<status> WPID=<pid> END
        w(marker_ws_addr, marker_ws.len() as u32, &mut out);
        w(status_addr, 4, &mut out);
        w(marker_wpid_addr, marker_wpid.len() as u32, &mut out);
        w(wpid_addr, 4, &mut out);
        w(marker_end_addr, marker_end.len() as u32, &mut out);
        // exit(0)
        out.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x31, 0xDB]);
        out.extend_from_slice(&[0xCD, 0x80]);
        out
    };

    let code_len = build_code(0, 0, 0, 0, 0).len() as u32;
    let marker_ws_addr = INIT_LOAD_ADDR + INIT_ENTRY_OFFSET + code_len;
    let marker_wpid_addr = marker_ws_addr + marker_ws.len() as u32;
    let marker_end_addr = marker_wpid_addr + marker_wpid.len() as u32;
    let status_addr = marker_end_addr + marker_end.len() as u32;
    let wpid_addr = status_addr + 4;
    let code = build_code(
        marker_ws_addr,
        marker_wpid_addr,
        marker_end_addr,
        status_addr,
        wpid_addr,
    );
    let zeros4 = [0u8; 4];
    let binary = make_init_elf32_safe(
        &code,
        &[marker_ws, marker_wpid, marker_end, &zeros4, &zeros4],
        7,
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

/// Wait-status milestone: /init forks a child that `exit(42)`s and
/// reaps it with `waitpid(-1, &status, 0)`, then reports the raw
/// status word and waitpid's return. Asserts `WEXITSTATUS == 42`,
/// `WIFEXITED` (status low 7 bits == 0), and a real child PID.
/// Pins child exit-status propagation + the kernel's status-word
/// `copy_to_user` — the piece the fork milestone leaves out by
/// passing `status = NULL`.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_wait_status_milestone() {
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
    let cpio = build_initramfs_wait_status();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — fork/waitpid likely failed; {}",
            dump_uart_on_failure(&cumulative, "wait-status")
        )
    });

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
    let read_at = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };

    let status = read_at(b"[USERSPACE WSTATUS=").unwrap_or_else(|| {
        panic!(
            "WSTATUS marker not found; {}",
            dump_uart_on_failure(&cumulative, "wait-status-marker")
        )
    });
    let wpid = read_at(b"WPID=").unwrap_or_else(|| {
        panic!(
            "WPID marker not found; {}",
            dump_uart_on_failure(&cumulative, "wait-pid-marker")
        )
    });
    eprintln!("waitpid returned pid={wpid}, raw status=0x{status:08x}");

    // WIFEXITED: low 7 bits of status are 0 for a normal exit.
    assert_eq!(
        status & 0x7f,
        0,
        "expected a normal-exit status (WIFEXITED), got raw 0x{status:08x}; {}",
        dump_uart_on_failure(&cumulative, "wait-not-exited")
    );
    // WEXITSTATUS: bits 8..16.
    let exit_code = (status >> 8) & 0xff;
    assert_eq!(
        exit_code,
        42,
        "expected WEXITSTATUS == 42, got {exit_code} (raw 0x{status:08x}); {}",
        dump_uart_on_failure(&cumulative, "wait-exitcode")
    );
    assert!(
        (2..0x8000_0000).contains(&wpid),
        "expected waitpid to return a real child PID (> 1), got {wpid}; {}",
        dump_uart_on_failure(&cumulative, "wait-pid")
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

/// File-backed mmap milestone: /init writes `"MAPDATA8"` to a file,
/// maps it `PROT_READ|MAP_PRIVATE`, and dumps the mapped bytes via
/// `write(1, mapped_addr, 8)`. The dump's `copy_from_user` is the
/// first touch of the mapping, so it faults the file page in from
/// the page cache (`filemap_fault`/`shmem_fault`) — a different
/// kernel path than anonymous mmap or COW, and the one `ld.so` uses
/// to map shared objects. Asserts the address is a valid
/// page-aligned userspace pointer and the mapped bytes equal the
/// file contents.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_file_mmap_milestone() {
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
    let cpio = build_initramfs_file_mmap();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — file-backed mmap may have \
             returned -errno and dumping the bad pointer SIGSEGV'd /init; {}",
            dump_uart_on_failure(&cumulative, "file-mmap")
        )
    });

    // Strip ONLCR so a 0x0A in the binary address survives intact.
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
    let find_after = |marker: &[u8]| -> Option<usize> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| p + marker.len())
    };

    let addr = find_after(b"[USERSPACE FMAP=")
        .and_then(|o| stripped.get(o..o + 4))
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or_else(|| {
            panic!(
                "FMAP marker not found; {}",
                dump_uart_on_failure(&cumulative, "file-mmap-addr")
            )
        });
    let data = find_after(b"DATA=")
        .and_then(|o| stripped.get(o..o + 8))
        .map(|s| s.to_vec())
        .unwrap_or_else(|| {
            panic!(
                "DATA marker not found; {}",
                dump_uart_on_failure(&cumulative, "file-mmap-data")
            )
        });
    eprintln!(
        "file-backed mmap addr = 0x{addr:08X}, mapped bytes = {:?}",
        String::from_utf8_lossy(&data)
    );

    const KERNEL_BASE: u32 = 0xC000_0000;
    assert!(
        (INIT_LOAD_ADDR..KERNEL_BASE).contains(&addr),
        "mmap addr 0x{addr:08X} outside userspace range; {}",
        dump_uart_on_failure(&cumulative, "file-mmap-range")
    );
    assert_eq!(
        addr & 0xFFF,
        0,
        "mmap addr 0x{addr:08X} not page-aligned; {}",
        dump_uart_on_failure(&cumulative, "file-mmap-align")
    );
    assert_eq!(
        data.as_slice(),
        b"MAPDATA8",
        "bytes read through the file mapping should equal the file contents \
         \"MAPDATA8\"; got {:?} (all-zero ⇒ the file page wasn't faulted in); {}",
        String::from_utf8_lossy(&data),
        dump_uart_on_failure(&cumulative, "file-mmap-bytes")
    );
}

/// Executable file-backed mmap milestone: /init writes a 3-byte
/// function (`int 0x80; ret`) to a file, maps it PROT_READ|PROT_EXEC,
/// and CALLs into the mapping. The mapped code — running from a
/// demand-faulted file page — executes the write syscall (emitting
/// `[USERSPACE XMAP]`) and returns. Asserts the mapping address is a
/// valid page-aligned userspace pointer and the XMAP marker (emitted
/// *by the mapped code*) appears, proving instruction fetch + execute
/// from a file-backed exec mapping — the mechanism `ld.so` uses to
/// run shared-object text.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_exec_mmap_milestone() {
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
    let cpio = build_initramfs_exec_mmap();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — the CALL into the exec \
             mapping likely faulted (exec mapping unsupported or page not faulted \
             in); {}",
            dump_uart_on_failure(&cumulative, "exec-mmap")
        )
    });

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
    let addr = stripped
        .windows(16)
        .position(|w| w == b"[USERSPACE FMAP=")
        .and_then(|p| stripped.get(p + 16..p + 20))
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or_else(|| {
            panic!(
                "FMAP marker not found; {}",
                dump_uart_on_failure(&cumulative, "exec-mmap-addr")
            )
        });
    let ran = stripped.windows(16).any(|w| w == b"[USERSPACE XMAP]");
    eprintln!("exec mmap addr = 0x{addr:08X}, mapped code ran (XMAP) = {ran}");

    const KERNEL_BASE: u32 = 0xC000_0000;
    assert!(
        (INIT_LOAD_ADDR..KERNEL_BASE).contains(&addr),
        "exec mmap addr 0x{addr:08X} outside userspace range (mmap likely \
         returned -errno); {}",
        dump_uart_on_failure(&cumulative, "exec-mmap-range")
    );
    assert_eq!(
        addr & 0xFFF,
        0,
        "exec mmap addr 0x{addr:08X} not page-aligned"
    );
    assert!(
        ran,
        "the XMAP marker (emitted by code executing from the file-backed exec \
         mapping) never appeared — instruction fetch from the mapped page \
         failed; {}",
        dump_uart_on_failure(&cumulative, "exec-mmap-ran")
    );
}

/// TLS milestone: /init sets up thread-local storage with
/// `set_thread_area` (sys 243), loads the returned selector into
/// `%gs`, and reads the TLS block's first word via `%gs:0`. Asserts
/// the syscall returned 0 and the `%gs:0` read equals the sentinel
/// (`0x12345678`) placed at the TLS base. Pins the full i386 TLS
/// chain glibc/musl use before `main`: GDT TLS-descriptor setup,
/// `mov gs` caching the descriptor base, and a segment-relative load
/// honouring that base.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_set_thread_area_milestone() {
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
    let cpio = build_initramfs_set_thread_area();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — set_thread_area or the \
             `mov gs`/`%gs:0` read likely faulted; {}",
            dump_uart_on_failure(&cumulative, "set-thread-area")
        )
    });

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
    let read_at = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };

    let sta = read_at(b"[USERSPACE STA=").map(|v| v as i32);
    let ent = read_at(b"ENT=").map(|v| v as i32);
    let tls = read_at(b"TLS=");
    eprintln!("set_thread_area ret={sta:?}, entry_number={ent:?}, gs:0 read={tls:#x?}");

    assert_eq!(
        sta,
        Some(0),
        "set_thread_area should return 0; got {sta:?}; {}",
        dump_uart_on_failure(&cumulative, "sta-ret")
    );
    assert_eq!(
        tls,
        Some(0x1234_5678),
        "reading %gs:0 should return the TLS sentinel 0x12345678 — proving the \
         GDT TLS descriptor base was installed and `mov gs` cached it; got \
         {tls:#x?}; {}",
        dump_uart_on_failure(&cumulative, "tls-read")
    );
}

/// Shared-memory milestone: /init maps a page MAP_SHARED|ANONYMOUS,
/// forks, the child writes `0x12345678` into it and exits, and the
/// parent (after waitpid) reads the same page. Asserts the parent
/// sees the child's write — which only holds for MAP_SHARED (under
/// MAP_PRIVATE/COW the parent would still read zero). Pins
/// shared-anonymous-memory semantics, distinct from the COW path.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_shared_mmap_milestone() {
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
    let cpio = build_initramfs_shared_mmap();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — shared mmap or fork/waitpid \
             likely failed; {}",
            dump_uart_on_failure(&cumulative, "shared-mmap")
        )
    });

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
    let shm = stripped
        .windows(15)
        .position(|w| w == b"[USERSPACE SHM=")
        .and_then(|p| stripped.get(p + 15..p + 19))
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or_else(|| {
            panic!(
                "SHM marker not found; {}",
                dump_uart_on_failure(&cumulative, "shm-marker")
            )
        });
    eprintln!("parent read shared page = {shm:#010x} (child wrote 0x12345678)");
    assert_eq!(
        shm,
        0x1234_5678,
        "parent should see the child's write through the MAP_SHARED page; got \
         {shm:#010x} (zero ⇒ the mapping behaved as private/COW, not shared); {}",
        dump_uart_on_failure(&cumulative, "shm-value")
    );
}

/// Futex milestone: /init forks; the child blocks in
/// `futex(FUTEX_WAIT)` on a shared word, the parent spins
/// `futex(FUTEX_WAKE)` until it wakes the waiter, then reaps it.
/// Asserts the child's WAIT returned 0 (woken, not -EAGAIN) and the
/// parent's WAKE returned 1 (exactly one task woken). The latter is
/// the decisive proof of the kernel-side futex wait-queue handoff —
/// the primitive under every pthread mutex/condvar.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_futex_milestone() {
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
    let cpio = build_initramfs_futex();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — futex WAIT never woke \
             (the parent's WAKE didn't find the blocked child, or WAIT didn't \
             block), so waitpid deadlocked; {}",
            dump_uart_on_failure(&cumulative, "futex")
        )
    });

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
    let read_at = |marker: &[u8]| -> Option<i32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
    };

    let woke = read_at(b"[USERSPACE WOKE=");
    let wake = read_at(b"WAKE=");
    eprintln!("futex: child WAIT ret={woke:?}, parent WAKE ret={wake:?}");
    assert_eq!(
        woke,
        Some(0),
        "child's FUTEX_WAIT should return 0 (woken), not -EAGAIN/-errno; got {woke:?}; {}",
        dump_uart_on_failure(&cumulative, "futex-wait")
    );
    assert_eq!(
        wake,
        Some(1),
        "parent's FUTEX_WAKE should report waking exactly 1 task (proving the \
         child was blocked in the futex wait-queue and got woken); got {wake:?}; {}",
        dump_uart_on_failure(&cumulative, "futex-wake")
    );
}

/// Clone-thread milestone: /init `clone(CLONE_VM)`s a task onto an
/// mmap'd stack; the new task writes a sentinel into a normal data
/// global, the parent busy-waits on it and reaps the child. Asserts
/// the parent read the sentinel — only possible if the two tasks
/// share one address space (CLONE_VM). Pins shared-address-space
/// thread creation, the `pthread_create` mechanism.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_clone_thread_milestone() {
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
    let cpio = build_initramfs_clone_thread();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — clone(CLONE_VM) likely \
             failed (the cloned task never ran, or didn't share the address \
             space, so the parent spun out / waitpid deadlocked); {}",
            dump_uart_on_failure(&cumulative, "clone-thread")
        )
    });

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
    let g = stripped
        .windows(18)
        .position(|w| w == b"[USERSPACE THREAD=")
        .and_then(|p| stripped.get(p + 18..p + 22))
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or_else(|| {
            panic!(
                "THREAD marker not found; {}",
                dump_uart_on_failure(&cumulative, "clone-marker")
            )
        });
    eprintln!("parent read shared global = {g:#010x} (cloned task wrote 0xABCDEF01)");
    assert_eq!(
        g,
        0xABCD_EF01,
        "parent should see the cloned task's write to the shared global; got \
         {g:#010x} (zero ⇒ CLONE_VM didn't share the address space); {}",
        dump_uart_on_failure(&cumulative, "clone-value")
    );
}

/// AF_UNIX socketpair milestone: /init `socketpair(AF_UNIX,
/// SOCK_STREAM, 0, &sv)`, writes "SOCK" on `sv[0]`, reads it from
/// `sv[1]`. Asserts socketpair returned 0, the two fds are distinct
/// and ≥ 3, both transfers returned 4, and the bytes round-tripped.
/// First test to touch the kernel socket subsystem (af_unix) — the
/// IPC backbone of D-Bus/X11/systemd.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_socketpair_milestone() {
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
    let cpio = build_initramfs_socketpair();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — socketpair likely returned \
             -errno (af_unix unsupported) or the read blocked; {}",
            dump_uart_on_failure(&cumulative, "socketpair")
        )
    });

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
    let read_at = |marker: &[u8]| -> Option<i32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
    };

    let spr = read_at(b"[USERSPACE SPR=");
    let s0 = read_at(b"S0=");
    let s1 = read_at(b"S1=");
    let wr = read_at(b"WR=");
    let rd = read_at(b"RD=");
    let buf = stripped
        .windows(4)
        .position(|w| w == b"BUF=")
        .and_then(|p| stripped.get(p + 4..p + 8))
        .map(|s| s.to_vec());
    eprintln!(
        "socketpair ret={spr:?} fds=[{s0:?},{s1:?}] write={wr:?} read={rd:?} buf={:?}",
        buf.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    );

    assert_eq!(
        spr,
        Some(0),
        "socketpair should return 0; got {spr:?}; {}",
        dump_uart_on_failure(&cumulative, "spr")
    );
    let s0 = s0.expect("S0 marker");
    let s1 = s1.expect("S1 marker");
    assert!(
        s0 >= 3 && s1 >= 3 && s0 != s1,
        "socketpair should yield two distinct fds ≥ 3; got [{s0}, {s1}]"
    );
    assert_eq!(wr, Some(4), "write should return 4; got {wr:?}");
    assert_eq!(rd, Some(4), "read should return 4; got {rd:?}");
    assert_eq!(
        buf.as_deref(),
        Some(&b"SOCK"[..]),
        "bytes read from the AF_UNIX socket should equal \"SOCK\"; got {:?}; {}",
        buf.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        dump_uart_on_failure(&cumulative, "sock-bytes")
    );
}

/// epoll milestone: /init makes a pipe readable, registers its read
/// end with `epoll_ctl(ADD, {EPOLLIN, cookie})`, and `epoll_wait`s.
/// Asserts the wait reported 1 ready fd, the events mask has EPOLLIN,
/// and — decisively — the 64-bit `data` cookie (0x0000CAFEDEADBEEF)
/// round-tripped from registration through the kernel back out, which
/// proves the eventpoll registration + readiness-report path. epoll
/// is the engine of every modern event loop.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_epoll_milestone() {
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
    let cpio = build_initramfs_epoll();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — epoll_create/ctl/wait \
             likely returned -errno or the wait blocked; {}",
            dump_uart_on_failure(&cumulative, "epoll")
        )
    });

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
    let u32_at = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };

    let epc = u32_at(b"[USERSPACE EPC=").map(|v| v as i32);
    let ctl = u32_at(b"CTL=").map(|v| v as i32);
    let wait = u32_at(b"WAIT=").map(|v| v as i32);
    let evt = u32_at(b"EVT=");
    let dat = stripped
        .windows(4)
        .position(|w| w == b"DAT=")
        .and_then(|p| stripped.get(p + 4..p + 12))
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    eprintln!("epoll: create={epc:?} ctl={ctl:?} wait={wait:?} events={evt:#x?} data={dat:#x?}");

    assert!(
        epc.map(|v| v >= 0).unwrap_or(false),
        "epoll_create1 should return a valid fd; got {epc:?}; {}",
        dump_uart_on_failure(&cumulative, "epoll-create")
    );
    assert_eq!(ctl, Some(0), "epoll_ctl(ADD) should return 0; got {ctl:?}");
    assert_eq!(
        wait,
        Some(1),
        "epoll_wait should report exactly 1 ready fd; got {wait:?}; {}",
        dump_uart_on_failure(&cumulative, "epoll-wait")
    );
    const EPOLLIN: u32 = 0x001;
    assert_eq!(
        evt.map(|e| e & EPOLLIN),
        Some(EPOLLIN),
        "the ready event should have EPOLLIN set; got {evt:#x?}"
    );
    assert_eq!(
        dat,
        Some(0x0000_CAFE_DEAD_BEEF),
        "the 64-bit data cookie should round-trip through the kernel; got \
         {dat:#x?}; {}",
        dump_uart_on_failure(&cumulative, "epoll-cookie")
    );
}

/// eventfd milestone: /init `eventfd2(0,0)`s a counter fd, `write`s
/// the 8-byte value 42 to add to the counter, then `read`s the
/// counter back. Asserts a valid fd, both transfers moved 8 bytes,
/// and the counter read equals 42 — pinning the eventfd subsystem
/// and its 8-byte add/drain protocol (the standard epoll wakeup fd).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_eventfd_milestone() {
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
    let cpio = build_initramfs_eventfd();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — eventfd2 likely returned \
             -errno or the read blocked; {}",
            dump_uart_on_failure(&cumulative, "eventfd")
        )
    });

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
    let i32_at = |marker: &[u8]| -> Option<i32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
    };

    let efd = i32_at(b"[USERSPACE EFD=");
    let wr = i32_at(b"WR=");
    let rd = i32_at(b"RD=");
    let cnt = stripped
        .windows(4)
        .position(|w| w == b"CNT=")
        .and_then(|p| stripped.get(p + 4..p + 12))
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()));
    eprintln!("eventfd: fd={efd:?} write={wr:?} read={rd:?} counter={cnt:?}");

    assert!(
        efd.map(|v| v >= 3).unwrap_or(false),
        "eventfd2 should return a valid fd ≥ 3; got {efd:?}; {}",
        dump_uart_on_failure(&cumulative, "eventfd-fd")
    );
    assert_eq!(
        wr,
        Some(8),
        "write to eventfd should move 8 bytes; got {wr:?}"
    );
    assert_eq!(
        rd,
        Some(8),
        "read from eventfd should move 8 bytes; got {rd:?}"
    );
    assert_eq!(
        cnt,
        Some(42),
        "eventfd counter read should equal the 42 written; got {cnt:?}; {}",
        dump_uart_on_failure(&cumulative, "eventfd-cnt")
    );
}

/// Milestone: boot a REAL, statically-linked i386 binary as /init —
/// genuine compiler+glibc-generated machine code, not a hand-assembled
/// syscall stub. Uses glibc's static `ldconfig` (37 KB) extracted from
/// the Tinycore rootfs; the path comes from `WWWVM_STATIC_INIT`
/// (default `/tmp/wwwvm-linux/static-ldconfig`) and the test SKIPS if
/// absent, exactly like the kernel-dependent milestones (see the
/// README build-deps note for how to extract it). To get one:
/// `7z e Core-current.iso boot/core.gz && zcat core.gz | cpio -id
/// sbin/ldconfig`.
///
/// This is the capstone integration test: a single binary exercises
/// the whole stack at once — the kernel's ELF loader on a real
/// ET_EXEC, glibc's static CRT startup (`__libc_start_main`), TLS via
/// `set_thread_area`, brk/mmap for the malloc arena, and real fs
/// syscalls. ldconfig scans the (absent) library dirs, prints its
/// genuine "skipping /lib …" diagnostics, and `exit(0)`s cleanly.
///
/// Asserts: /init was exec'd, ldconfig's real output appeared
/// (`skipping /lib`, proving glibc + ldconfig's `main` ran), and it
/// exited with code 0 (the clean-exit init panic). This proves the
/// emulator runs actual compiled+linked Linux software.
#[test]
#[ignore = "requires WWWVM_STATIC_INIT (real static glibc binary); ~60s"]
fn linux_userspace_real_static_binary_milestone() {
    let kpath =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let kbytes = match std::fs::read(&kpath) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {kpath}: {e}");
            return;
        }
    };
    let binpath = std::env::var("WWWVM_STATIC_INIT")
        .unwrap_or_else(|_| "/tmp/wwwvm-linux/static-ldconfig".to_string());
    let initbin = match std::fs::read(&binpath) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read static init {binpath}: {e}");
            return;
        }
    };
    eprintln!("static /init = {binpath} ({} bytes)", initbin.len());

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let cpio = build_cpio_archive(&initbin, /* proc_dir */ true);
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    // The clean-exit init panic; ldconfig prints its output and
    // exit(0)s before this. We stop here so we don't run on into the
    // post-panic reboot stub (which hits an unimplemented opcode).
    let exit_marker = "Attempted to kill init! exitcode=0x00000000";
    let mut cumulative = Vec::<u8>::new();
    let chunk = 10_000_000u32;
    let budget = 8_000_000_000u64;
    let mut steps = 0u64;
    let stop_reason: String;
    loop {
        let (s, stop) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        let out = vm.drain_output();
        if !out.is_empty() {
            cumulative.extend_from_slice(&out);
        }
        if String::from_utf8_lossy(&cumulative).contains(exit_marker) {
            stop_reason = "reached clean-exit panic".to_string();
            break;
        }
        match stop {
            wwwvm_vm::Stop::CpuError(e) => {
                stop_reason = format!("CpuError: {e}");
                break;
            }
            wwwvm_vm::Stop::Halted => {
                stop_reason = "Halted".to_string();
                break;
            }
            wwwvm_vm::Stop::StepBudget => {
                if steps >= budget {
                    stop_reason = "step budget exhausted".to_string();
                    break;
                }
            }
        }
    }

    let text = String::from_utf8_lossy(&cumulative);
    eprintln!("=== real static /init milestone ===");
    eprintln!("  steps run: {steps}");
    eprintln!("  stop reason: {stop_reason}");
    let reached = text.contains("Run /init as init process");
    let ldconfig_ran = text.contains("skipping /lib");
    let clean_exit = text.contains(exit_marker);
    eprintln!("  reached /init exec: {reached}");
    eprintln!("  ldconfig output (skipping /lib): {ldconfig_ran}");
    eprintln!("  clean exit (exitcode=0): {clean_exit}");
    for needle in ["segfault at", "trap invalid opcode", "Kernel panic"] {
        if let Some(p) = text.find(needle) {
            let endp = text[p..]
                .find('\n')
                .map(|n| p + n)
                .unwrap_or((p + 120).min(text.len()));
            eprintln!("  [{needle}] {:?}", &text[p..endp]);
        }
    }

    assert!(
        reached,
        "the kernel never exec'd /init — the real ELF didn't load; {}",
        dump_uart_on_failure(&cumulative, "real-static-exec")
    );
    assert!(
        ldconfig_ran,
        "ldconfig's real output (\"skipping /lib\") never appeared — glibc \
         static startup or ldconfig's main didn't run; {}",
        dump_uart_on_failure(&cumulative, "real-static-output")
    );
    assert!(
        clean_exit,
        "the binary did not exit cleanly with code 0 (no exitcode=0x00000000 \
         init panic) — it likely crashed mid-run; {}",
        dump_uart_on_failure(&cumulative, "real-static-exit")
    );
}

/// Diagnostic: boot a DYNAMICALLY-linked i386 binary as /init,
/// forcing the full `ld.so` path. The kernel execs /init, sees its
/// PT_INTERP=/lib/ld-linux.so.2, loads that interpreter, which then
/// mmaps libc.so.6 to satisfy DT_NEEDED, relocates, and jumps to the
/// program. We use Tinycore's `rotdash` (1.7 KB, needs only libc) as
/// the minimal case. Files come from `WWWVM_DYN_ROOTFS` (default
/// `/tmp/wwwvm-linux/rootfs`, the extracted Tinycore rootfs); the
/// test SKIPS if absent. ALWAYS PASSES — it reports how far the
/// dynamic linker got (reached /init, ld.so output, any program
/// output, a CpuError on an unimplemented instruction, a segfault)
/// so the next tick knows exactly what (if anything) ld.so needs that
/// the emulator/kernel path is missing.
#[test]
#[ignore = "diagnostic: boots a dynamically-linked glibc binary via ld.so; ~bounded"]
fn linux_userspace_dynamic_binary_diag() {
    let kpath =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let kbytes = match std::fs::read(&kpath) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {kpath}: {e}");
            return;
        }
    };
    let root =
        std::env::var("WWWVM_DYN_ROOTFS").unwrap_or_else(|_| "/tmp/wwwvm-linux/rootfs".to_string());
    let read_or_skip = |rel: &str| -> Option<Vec<u8>> {
        match std::fs::read(format!("{root}/{rel}")) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("skipping: read {root}/{rel}: {e}");
                None
            }
        }
    };
    let (Some(initbin), Some(ld), Some(libc)) = (
        read_or_skip("usr/bin/rotdash"),
        read_or_skip("lib/ld-linux.so.2"),
        read_or_skip("lib/libc.so.6"),
    ) else {
        return;
    };
    eprintln!(
        "dynamic /init = rotdash ({} B), ld.so {} B, libc {} B",
        initbin.len(),
        ld.len(),
        libc.len()
    );

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let cpio = build_cpio_with_libs(&initbin, &[("ld-linux.so.2", &ld), ("libc.so.6", &libc)]);
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut cumulative = Vec::<u8>::new();
    let chunk = 10_000_000u32;
    let budget = 8_000_000_000u64;
    let mut steps = 0u64;
    let stop_reason: String;
    loop {
        let (s, stop) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        let out = vm.drain_output();
        if !out.is_empty() {
            cumulative.extend_from_slice(&out);
        }
        match stop {
            wwwvm_vm::Stop::CpuError(e) => {
                stop_reason = format!("CpuError: {e}");
                break;
            }
            wwwvm_vm::Stop::Halted => {
                stop_reason = "Halted".to_string();
                break;
            }
            wwwvm_vm::Stop::StepBudget => {
                if steps >= budget {
                    stop_reason = "step budget exhausted".to_string();
                    break;
                }
            }
        }
    }

    let text = String::from_utf8_lossy(&cumulative);
    eprintln!("=== dynamic /init diagnostic ===");
    eprintln!("  steps run: {steps}");
    eprintln!("  stop reason: {stop_reason}");
    eprintln!(
        "  reached /init exec: {}",
        text.contains("Run /init as init process")
    );
    for needle in [
        "Run /init as init process",
        "segfault at",
        "trap invalid opcode",
        "trap ",
        "Kernel panic",
        "exitcode=",
        "error while loading shared libraries",
        "not found",
    ] {
        if let Some(p) = text.find(needle) {
            let endp = text[p..]
                .find('\n')
                .map(|n| p + n)
                .unwrap_or((p + 120).min(text.len()));
            eprintln!("  [{needle}] {:?}", &text[p..endp]);
        }
    }
    let tail = &cumulative[cumulative.len().saturating_sub(700)..];
    eprintln!("  --- last {} bytes of UART ---", tail.len());
    eprintln!("{}", String::from_utf8_lossy(tail));
}

/// Diagnostic: attempt a real DYNAMICALLY-linked program end to end.
/// A tiny hand-assembled /init `execve`s `/bin/busybox` with argv
/// `["busybox", "echo", "DYNLINK_OK"]`; the kernel loads busybox, its
/// interpreter `/lib/ld-linux.so.2` mmaps libc/libm/libcrypt,
/// relocates, and (if it all works) runs busybox's `echo` applet,
/// which prints `DYNLINK_OK`.
///
/// STATUS (2026-05-29): NOT yet working — `ld.so` null-derefs during
/// dynamic linking (`busybox[1]: segfault at 0 ip b7f21e9d`, faulting
/// insn `mov eax,[eax]` with eax=0, in a relocation/list-walk loop).
/// Static binaries run fine (`real_static_binary_milestone`); the
/// dynamic path has a deeper bug — see the README blocker. This stays
/// a diagnostic (ALWAYS PASSES, logs how far ld.so got) until fixed,
/// so the --ignored regression sweep doesn't fail on a known gap.
///
/// busybox + the four libraries are read from `WWWVM_DYN_ROOTFS`
/// (default `/tmp/wwwvm-linux/rootfs`); the test SKIPS if absent. See
/// README build-deps for how to extract the Tinycore rootfs.
#[test]
#[ignore = "diagnostic: dynamic-linking attempt (known WIP); ~bounded"]
fn linux_userspace_dynamic_exec_diag() {
    let kpath =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let kbytes = match std::fs::read(&kpath) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {kpath}: {e}");
            return;
        }
    };
    let root =
        std::env::var("WWWVM_DYN_ROOTFS").unwrap_or_else(|_| "/tmp/wwwvm-linux/rootfs".to_string());
    let read_or_skip = |rel: &str| -> Option<Vec<u8>> {
        match std::fs::read(format!("{root}/{rel}")) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("skipping: read {root}/{rel}: {e}");
                None
            }
        }
    };
    let (Some(busybox), Some(ld), Some(libc), Some(libm), Some(libcrypt)) = (
        read_or_skip("bin/busybox"),
        read_or_skip("lib/ld-linux.so.2"),
        read_or_skip("lib/libc.so.6"),
        read_or_skip("lib/libm.so.6"),
        read_or_skip("lib/libcrypt.so.1"),
    ) else {
        return;
    };

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let init = build_init_execve_busybox();
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
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut cumulative = Vec::<u8>::new();
    let chunk = 10_000_000u32;
    let budget = 10_000_000_000u64;
    let mut steps = 0u64;
    let mut found = false;
    let stop_reason: String;
    loop {
        let (s, stop) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        let out = vm.drain_output();
        if !out.is_empty() {
            cumulative.extend_from_slice(&out);
        }
        if String::from_utf8_lossy(&cumulative).contains("DYNLINK_OK") {
            found = true;
            stop_reason = "found DYNLINK_OK".to_string();
            break;
        }
        match stop {
            wwwvm_vm::Stop::CpuError(e) => {
                stop_reason = format!("CpuError: {e}");
                break;
            }
            wwwvm_vm::Stop::Halted => {
                stop_reason = "Halted".to_string();
                break;
            }
            wwwvm_vm::Stop::StepBudget => {
                if steps >= budget {
                    stop_reason = "step budget exhausted".to_string();
                    break;
                }
            }
        }
    }

    let text = String::from_utf8_lossy(&cumulative);
    eprintln!("=== dynamic exec milestone ===");
    eprintln!("  steps run: {steps}");
    eprintln!("  stop reason: {stop_reason}");
    eprintln!("  DYNLINK_OK seen: {found}");
    for needle in [
        "[EXECVE-FAIL]",
        "error while loading shared libraries",
        "segfault at",
        "trap invalid opcode",
        "Kernel panic",
    ] {
        if let Some(p) = text.find(needle) {
            let endp = text[p..]
                .find('\n')
                .map(|n| p + n)
                .unwrap_or((p + 120).min(text.len()));
            eprintln!("  [{needle}] {:?}", &text[p..endp]);
        }
    }
    if !found {
        let tail = &cumulative[cumulative.len().saturating_sub(700)..];
        eprintln!("  --- last {} bytes of UART ---", tail.len());
        eprintln!("{}", String::from_utf8_lossy(tail));
    }

    // Diagnostic, not an assertion: dynamic linking is known-WIP (ld.so
    // null-derefs during relocation). When `found` becomes true this can
    // be promoted to an asserting milestone.
    if found {
        eprintln!("  ✓ DYNLINK_OK — dynamic linking works end to end!");
    } else {
        eprintln!("  ✗ dynamic linking did not complete (see README blocker)");
    }
}

/// Diagnostic isolating the dynamic-linking failure: boots busybox
/// DIRECTLY as /init (kernel-exec'd, not via an execve stub) with all
/// four needed libs in /lib. Compared to `dynamic_exec_diag` (busybox
/// via an execve stub) and `dynamic_binary_diag` (rotdash, kernel-
/// exec'd, libc-only → reaches a clean exit), this controls for the
/// execve-stub variable: if busybox-direct still segfaults in ld.so's
/// relocation walk, the stub is innocent and the bug is in the
/// multi-library link path itself. Reports the faulting ip so it can
/// be compared against the stub run's `b7f21e9d`. ALWAYS PASSES.
#[test]
#[ignore = "diagnostic: busybox kernel-exec'd to isolate ld.so bug; ~bounded"]
fn linux_userspace_busybox_direct_diag() {
    let kpath =
        std::env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-linux/vmlinuz".to_string());
    let kbytes = match std::fs::read(&kpath) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: read {kpath}: {e}");
            return;
        }
    };
    let root =
        std::env::var("WWWVM_DYN_ROOTFS").unwrap_or_else(|_| "/tmp/wwwvm-linux/rootfs".to_string());
    let rd = |rel: &str| -> Option<Vec<u8>> {
        match std::fs::read(format!("{root}/{rel}")) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("skipping: read {root}/{rel}: {e}");
                None
            }
        }
    };
    let (Some(busybox), Some(ld), Some(libc), Some(libm), Some(libcrypt)) = (
        rd("bin/busybox"),
        rd("lib/ld-linux.so.2"),
        rd("lib/libc.so.6"),
        rd("lib/libm.so.6"),
        rd("lib/libcrypt.so.1"),
    ) else {
        return;
    };

    let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
    let bz = vm.load_bzimage(&kbytes).expect("load_bzimage");
    vm.set_kernel_cmdline(
        "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 \
         debug loglevel=8 ignore_loglevel",
    );
    let cpio = build_cpio_with_libs(
        &busybox,
        &[
            ("ld-linux.so.2", &ld),
            ("libc.so.6", &libc),
            ("libm.so.6", &libm),
            ("libcrypt.so.1", &libcrypt),
        ],
    );
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let mut cumulative = Vec::<u8>::new();
    let chunk = 10_000_000u32;
    let budget = 6_000_000_000u64;
    let mut steps = 0u64;
    let stop_reason: String;
    loop {
        let (s, stop) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        let out = vm.drain_output();
        if !out.is_empty() {
            cumulative.extend_from_slice(&out);
        }
        let t = String::from_utf8_lossy(&cumulative);
        if t.contains("segfault at") || t.contains("Attempted to kill init") {
            stop_reason = "segfault/panic".to_string();
            break;
        }
        match stop {
            wwwvm_vm::Stop::CpuError(e) => {
                stop_reason = format!("CpuError: {e}");
                break;
            }
            wwwvm_vm::Stop::Halted => {
                stop_reason = "Halted".to_string();
                break;
            }
            wwwvm_vm::Stop::StepBudget => {
                if steps >= budget {
                    stop_reason = "budget (likely past relocation, in init applet)".to_string();
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&cumulative);
    eprintln!("=== busybox-direct diagnostic ===");
    eprintln!("  steps: {steps}, stop: {stop_reason}");
    eprintln!(
        "  reached /init: {}",
        text.contains("Run /init as init process")
    );
    for needle in ["segfault at", "Code:", "Kernel panic", "exitcode="] {
        if let Some(p) = text.find(needle) {
            let endp = text[p..]
                .find('\n')
                .map(|n| p + n)
                .unwrap_or((p + 140).min(text.len()));
            eprintln!("  [{needle}] {:?}", &text[p..endp]);
        }
    }
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

/// Signal-delivery milestone: /init installs a SIGUSR1 handler
/// via `sys_rt_sigaction`, sends SIGUSR1 to itself via `sys_kill`.
/// Handler writes `[USERSPACE HANDLER]\n` then calls `sys_exit(0)`
/// directly (skipping sigreturn, which is known broken in our
/// emulation — see commit history for the diagnostic). Test
/// verifies the HANDLER marker appears — pins the one-way signal
/// delivery path: sigaction stored the handler in
/// `current->sighand`, kill queued the signal, kernel delivered
/// to the handler's address.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_signal_milestone() {
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
    let cpio = build_initramfs_signal();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let handler_marker: &[u8] = b"[USERSPACE HANDLER]";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, handler_marker, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE HANDLER]` not seen in 16 B steps — signal delivery broken \
                 (sigaction returned -errno, or kill failed, or kernel never jumped \
                 to the handler); {}",
                dump_uart_on_failure(&cumulative, "signal")
            )
        });
    eprintln!("HANDLER marker after {steps} steps — signal delivery confirmed");
}

/// Milestone: the full signal round-trip via `rt_sigreturn`.
/// `build_initramfs_signal_rt` installs a SIGUSR1 handler with
/// `SA_RESTORER | SA_SIGINFO`, sends itself the signal, and the
/// handler RETURNS via `ret` (instead of exiting). That pops the
/// kernel-pushed pretcode → runs the restorer → `rt_sigreturn(173)`
/// → the kernel restores the pre-signal context from the
/// rt_sigframe → `main` resumes past the kill, writes
/// `[USERSPACE DONE]`, and exits cleanly.
///
/// What this pins beyond the one-way `signal_milestone`:
///   * `setup_rt_frame` builds a correct rt_sigframe on the user
///     stack (saved sigcontext inside `uc.uc_mcontext`),
///   * the cross-ring `ret` → restorer transition,
///   * `sys_rt_sigreturn` restoring EIP/ESP/EFLAGS/segregs and the
///     GP registers, so userspace resumes exactly where the signal
///     interrupted it.
///
/// History: this was long recorded as the "sys_rt_sigreturn
/// segfaults" blocker. Root cause (found 2026-05-29 via a raw frame
/// dump) was a test bug — the handler was registered without
/// `SA_SIGINFO`, so the kernel built a *legacy* sigframe, but the
/// restorer called `rt_sigreturn`, which reads the rt_sigframe
/// layout from the wrong offset → zero EIP/ESP → SIGSEGV. Adding
/// `SA_SIGINFO` (matching frame type to restorer) fixes it; the
/// emulator's signal machinery was correct all along.
#[test]
#[ignore = "boots a real Linux bzImage; full signal round-trip; ~55s wall-clock"]
fn linux_userspace_sigreturn_milestone() {
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
    let cpio = build_initramfs_signal_rt();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let handler_marker: &[u8] = b"[USERSPACE HANDLER]";
    let done_marker: &[u8] = b"[USERSPACE DONE]";
    let mut cumulative = Vec::<u8>::new();
    let handler_ran =
        run_until_marker(&mut vm, handler_marker, 16_000_000_000, &mut cumulative).is_ok();
    // After the handler returns, look for DONE (main resumed) — bound
    // the failure case (SIGSEGV → no DONE) to a couple extra B steps.
    let resumed = run_until_marker(&mut vm, done_marker, 2_000_000_000, &mut cumulative).is_ok();

    let text = String::from_utf8_lossy(&cumulative);
    let segv = text.contains("exitcode=0x0000000b");
    let clean_exit = text.contains("exitcode=0x00000000");
    eprintln!("=== sigreturn round-trip milestone ===");
    eprintln!("  handler ran  = {handler_ran}");
    eprintln!("  main resumed (DONE) = {resumed}");
    eprintln!("  SIGSEGV (exitcode=0x0b) seen = {segv}");
    eprintln!("  clean exit (exitcode=0) seen = {clean_exit}");
    // The kernel (show_unhandled_signals=1 by default) prints the
    // faulting ip/sp/error before killing init — that pins exactly
    // where the sigreturn path dies.
    for needle in [
        "segfault at",
        "trap ",
        "general protection",
        "BUG:",
        "Code:",
        "Kernel panic",
    ] {
        if let Some(p) = text.find(needle) {
            let endp = text[p..]
                .find('\n')
                .map(|n| p + n)
                .unwrap_or((p + 160).min(text.len()));
            eprintln!("  [{needle}] {:?}", &text[p..endp]);
        }
    }

    assert!(
        handler_ran,
        "signal was never delivered to the handler — {}",
        dump_uart_on_failure(&cumulative, "sigreturn")
    );
    assert!(
        !segv,
        "the sigreturn path SIGSEGV'd (exitcode=0x0b) — rt_sigreturn restored \
         a bad context. {}",
        dump_uart_on_failure(&cumulative, "sigreturn")
    );
    assert!(
        resumed,
        "handler ran but `main` never resumed past the kill (no DONE marker) — \
         rt_sigreturn did not return to the interrupted context. {}",
        dump_uart_on_failure(&cumulative, "sigreturn")
    );
}

/// Diagnostic that dumps the raw rt_sigframe the kernel built on the
/// user stack (via `build_initramfs_signal_framedump`). Decodes
/// `pretcode`/`sig`/pointers and scans every dword for non-zero
/// values, so we can see whether `setup_rt_frame` actually saved the
/// pre-signal registers or left the frame demand-zero. ALWAYS PASSES.
#[test]
#[ignore = "diagnostic: dumps the signal frame; ~55s wall-clock"]
fn linux_userspace_sigframe_dump_diag() {
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
    let cpio = build_initramfs_signal_framedump();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let reached = run_until_marker(&mut vm, end, 8_000_000_000, &mut cumulative).is_ok();
    eprintln!("=== sigframe dump diagnostic (reached_end={reached}) ===");

    // Strip ONLCR (\r\n -> \n) to undo the TTY's LF translation of
    // any 0x0A bytes in the binary frame.
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
    let m: &[u8] = b"[USERSPACE FRAME=";
    let Some(p) = stripped.windows(m.len()).position(|w| w == m) else {
        eprintln!("  FRAME marker not found");
        return;
    };
    let frame_start = p + m.len();
    // The frame ends at the post marker; clamp to 256.
    let end_rel = stripped[frame_start..]
        .windows(2)
        .position(|w| w == b"][")
        .unwrap_or(256)
        .min(256);
    let frame = &stripped[frame_start..frame_start + end_rel];
    eprintln!("  captured {} frame bytes", frame.len());
    // Decode the leading rt_sigframe fields.
    let dw = |off: usize| -> Option<u32> {
        frame
            .get(off..off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };
    eprintln!("  pretcode (+0)  = {:#010x?}", dw(0));
    eprintln!("  sig      (+4)  = {:?}", dw(4));
    eprintln!("  pinfo    (+8)  = {:#010x?}", dw(8));
    eprintln!("  puc      (+12) = {:#010x?}", dw(12));
    // Dump all non-zero dwords with their offsets — the saved EIP
    // (a 0x0804_80xx code address) and ESP (a stack address) should
    // appear if setup_rt_frame populated the sigcontext.
    eprint!("  non-zero dwords:");
    let mut n = 0;
    for off in (0..frame.len().saturating_sub(3)).step_by(4) {
        if let Some(v) = dw(off) {
            if v != 0 {
                eprint!(" +{off}={v:#x}");
                n += 1;
            }
        }
    }
    eprintln!();
    eprintln!("  ({n} non-zero dwords of {} total)", frame.len() / 4);
}

/// `sys_mprotect` milestone: /init mmap's a page R+W, writes
/// `0x42`, calls `sys_mprotect(addr, 4096, PROT_READ)`, reads
/// the byte back. Test asserts mprotect ret == 0 AND byte == 0x42.
/// Pins kernel's per-VMA `vm_flags` mutation by mprotect and
/// the fact that data is preserved through a permission change.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_mprotect_milestone() {
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
    let cpio = build_initramfs_mprotect();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE MPROT_RET=";
    let marker_mid: &[u8] = b"BYTE=";
    let marker_end_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "mprotect")
            )
        });
    eprintln!("mprotect milestone end marker after {steps} steps");

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
    let ret_off = pre_pos + marker_pre.len();
    let mprotect_ret = u32::from_le_bytes(stripped[ret_off..ret_off + 4].try_into().unwrap());

    let mid_pos = stripped
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid");
    let byte = stripped[mid_pos + marker_mid.len()];

    eprintln!("mprotect ret = 0x{mprotect_ret:08X}, byte read back = 0x{byte:02X}");
    assert_eq!(
        mprotect_ret,
        0,
        "expected mprotect to return 0, got 0x{mprotect_ret:08X}; {}",
        dump_uart_on_failure(&cumulative, "mprotect-ret")
    );
    assert_eq!(
        byte,
        0x42,
        "expected byte read after mprotect-to-RO to equal sentinel 0x42, \
         got 0x{byte:02X} — data lost across permission change; {}",
        dump_uart_on_failure(&cumulative, "mprotect-byte")
    );
}

/// `sys_sysinfo` milestone: /init calls `sys_sysinfo(&buf)`,
/// reads `uptime` (first field, 4 bytes). Test asserts uptime
/// is positive. Pins the kernel's sysinfo path — fills the
/// 64-byte struct with uptime + load + memory + procs.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_sysinfo_milestone() {
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
    let cpio = build_initramfs_sysinfo();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE UPTIME=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "sysinfo")
            )
        });
    eprintln!("sysinfo milestone end marker after {steps} steps");

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
    let up_off = pre_pos + marker_pre.len();
    let uptime = u32::from_le_bytes(stripped[up_off..up_off + 4].try_into().unwrap());
    eprintln!("sysinfo returned uptime = {uptime} seconds");
    assert!(
        uptime > 0,
        "expected uptime > 0 (kernel has been running for at least 1 second), \
         got {uptime}; if 0, sysinfo silently failed or kernel time hasn't \
         advanced; {}",
        dump_uart_on_failure(&cumulative, "sysinfo-zero")
    );
}

/// `sys_fcntl` milestone: /init opens a file, sets FD_CLOEXEC
/// via F_SETFD, reads back via F_GETFD. Test asserts the
/// returned flags equal `FD_CLOEXEC = 1`. Pins fd-flag storage
/// in the file-descriptor table entry (distinct from the
/// open-file flags that O_CREAT etc. live in).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_fcntl_milestone() {
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
    let cpio = build_initramfs_fcntl();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE FD_FLAGS=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "fcntl")
            )
        });
    eprintln!("fcntl milestone end marker after {steps} steps");

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
    let flags_off = pre_pos + marker_pre.len();
    let flags = u32::from_le_bytes(stripped[flags_off..flags_off + 4].try_into().unwrap());
    eprintln!("fcntl F_GETFD returned flags = 0x{flags:08X}");
    assert_eq!(
        flags,
        1,
        "expected fd_flags = FD_CLOEXEC (1), got 0x{flags:08X}; if 0, F_SETFD \
         silently failed; if some other bit, kernel has additional fd flags; {}",
        dump_uart_on_failure(&cumulative, "fcntl-wrong")
    );
}

/// `sys_symlink` + `sys_readlink` milestone: /init creates a
/// symlink `/link` → `/target`, reads it back, prints the target.
/// Test asserts the round-trip equals `b"/target"`. Pins symlink
/// inode creation (S_IFLNK) AND readlink returning the body
/// string (not following the link).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_symlink_milestone() {
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
    let cpio = build_initramfs_symlink();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_sym: &[u8] = b"[USERSPACE SYM=";
    let marker_rl: &[u8] = b"RL=";
    let marker_link: &[u8] = b"LINK=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "symlink")
            )
        });
    eprintln!("symlink milestone end marker after {steps} steps");

    // Binary 4-byte returns may contain 0x0A → ONLCR-padded; strip.
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

    let sym_pos = stripped
        .windows(marker_sym.len())
        .position(|w| w == marker_sym)
        .expect("marker_sym");
    let sym_off = sym_pos + marker_sym.len();
    let sym_ret = u32::from_le_bytes(stripped[sym_off..sym_off + 4].try_into().unwrap());

    let rl_pos = stripped
        .windows(marker_rl.len())
        .position(|w| w == marker_rl)
        .expect("marker_rl");
    let rl_off = rl_pos + marker_rl.len();
    let rl_ret = u32::from_le_bytes(stripped[rl_off..rl_off + 4].try_into().unwrap());

    let link_pos = stripped
        .windows(marker_link.len())
        .position(|w| w == marker_link)
        .expect("marker_link");
    let link_off = link_pos + marker_link.len();
    let link_bytes: [u8; 7] = stripped[link_off..link_off + 7].try_into().unwrap();

    eprintln!(
        "sys_symlink ret = 0x{sym_ret:08X} ({}), sys_readlink ret = 0x{rl_ret:08X} ({}), \
         link bytes = {:?}",
        sym_ret as i32,
        rl_ret as i32,
        std::str::from_utf8(&link_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        sym_ret,
        0,
        "sys_symlink returned 0x{sym_ret:08X} ({}) — expected 0; {}",
        sym_ret as i32,
        dump_uart_on_failure(&cumulative, "symlink-sym-ret")
    );
    assert_eq!(
        rl_ret,
        7,
        "sys_readlink returned 0x{rl_ret:08X} ({}) — expected 7 (len of \"/target\"); {}",
        rl_ret as i32,
        dump_uart_on_failure(&cumulative, "symlink-rl-ret")
    );
    assert_eq!(
        &link_bytes,
        b"/target",
        "expected readlink content b\"/target\", got {:02X?}; {}",
        link_bytes,
        dump_uart_on_failure(&cumulative, "symlink-wrong")
    );
}

/// `sys_uname` milestone: /init calls `sys_uname` and writes the
/// `sysname` field, which must be "Linux". Doubles as validation
/// that `make_init_elf32_safe` neutralizes the historical
/// {600,602}-bad-size hang that the raw `build_initramfs_uname`
/// (the original trigger of the bisection saga) reproduces.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_uname_milestone() {
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
    let cpio = build_initramfs_uname_safe();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE UNAME=";
    // sysname is "Linux\0\0..." (65 bytes), then the kernel TTY
    // emits the marker_post. Search for the marker_pre + "Linux".
    let needle: &[u8] = b"[USERSPACE UNAME=Linux";
    let mut cumulative = Vec::<u8>::new();
    let steps =
        run_until_marker(&mut vm, needle, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
            let saw_pre = cumulative
                .windows(marker_pre.len())
                .any(|w| w == marker_pre);
            let cause = if saw_pre {
                "marker appeared but sysname != \"Linux\" — uname filled a wrong/empty sysname"
            } else {
                "no UNAME marker — uname may have hung (bad /init size?) or returned -errno"
            };
            panic!(
                "`[USERSPACE UNAME=Linux` not seen in 16 B steps; {cause}; {}",
                dump_uart_on_failure(&cumulative, "uname")
            )
        });
    eprintln!("uname milestone — sysname=\"Linux\" confirmed after {steps} steps");
}

/// `sys_link` (hard link) milestone: /init writes `/a`, hard-links
/// it to `/b`, reads `/b` back. Test asserts link_ret == 0 and the
/// content equals `b"HARDLINK"` — proving `/b` shares `/a`'s inode.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_hardlink_milestone() {
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
    let cpio = build_initramfs_hardlink();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE LINK_RET=";
    let marker_mid: &[u8] = b"DATA=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — link or the read-back \
                 open may have failed; {}",
                dump_uart_on_failure(&cumulative, "hardlink")
            )
        });
    eprintln!("hardlink milestone end marker after {steps} steps");

    // link_ret is binary u32 (may contain 0x0A) → strip ONLCR.
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
    let ret_off = pre_pos + marker_pre.len();
    let link_ret = u32::from_le_bytes(stripped[ret_off..ret_off + 4].try_into().unwrap());

    let mid_pos = stripped
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid");
    let data_off = mid_pos + marker_mid.len();
    let data: [u8; 8] = stripped[data_off..data_off + 8].try_into().unwrap();

    eprintln!(
        "sys_link ret = 0x{link_ret:08X} ({}), content via /b = {:?}",
        link_ret as i32,
        std::str::from_utf8(&data).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        link_ret,
        0,
        "sys_link returned 0x{link_ret:08X} ({}) — expected 0; {}",
        link_ret as i32,
        dump_uart_on_failure(&cumulative, "hardlink-ret")
    );
    assert_eq!(
        &data,
        b"HARDLINK",
        "expected content via /b = b\"HARDLINK\" (shared inode), got {:02X?}; {}",
        data,
        dump_uart_on_failure(&cumulative, "hardlink-wrong")
    );
}

/// `sys_getdents64` milestone: /init mkdir's `/d`, creates
/// `/d/ZZMARKER`, opens `/d` and calls getdents64. Test asserts
/// the returned byte count > 0 and the dirent buffer contains the
/// byte string `b"ZZMARKER"` (the d_name of the created file).
/// Pins directory enumeration — the primitive `ls`/`readdir` need.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_getdents_milestone() {
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
    let cpio = build_initramfs_getdents();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE DENTS=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — getdents64 or the dir \
                 open may have failed; {}",
                dump_uart_on_failure(&cumulative, "getdents")
            )
        });
    eprintln!("getdents milestone end marker after {steps} steps");

    // The dirent buffer is binary (d_ino/d_off may contain 0x0A
    // that the kernel TTY ONLCR-pads). Strip `\r\n` → `\n` so the
    // 4-byte count decodes correctly. The "ZZMARKER" filename has
    // no 0x0A, so it survives intact and is searchable in either
    // form; we search the stripped buffer for consistency.
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
    let n_off = pre_pos + marker_pre.len();
    let n = u32::from_le_bytes(stripped[n_off..n_off + 4].try_into().unwrap());
    let has_name = stripped.windows(8).any(|w| w == b"ZZMARKER")
        || cumulative.windows(8).any(|w| w == b"ZZMARKER");
    eprintln!("getdents64 returned n = {n} bytes; ZZMARKER present = {has_name}");
    assert!(
        (n as i32) > 0,
        "expected getdents64 to return a positive byte count, got {} ({}); {}",
        n,
        n as i32,
        dump_uart_on_failure(&cumulative, "getdents-count")
    );
    assert!(
        has_name,
        "expected the dirent buffer to contain the filename b\"ZZMARKER\" (n={n}); \
         directory enumeration didn't surface the created entry; {}",
        dump_uart_on_failure(&cumulative, "getdents-name")
    );
}

/// `sys_statfs` milestone: /init calls `sys_statfs("/", &buf)`,
/// reads the first 4 bytes (f_type). Test asserts they equal
/// `TMPFS_MAGIC = 0x01021994` — proves rootfs is tmpfs AND the
/// kernel filled the statfs struct.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_statfs_milestone() {
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
    let cpio = build_initramfs_statfs();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE FS_TYPE=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "statfs")
            )
        });
    eprintln!("statfs milestone end marker after {steps} steps");

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
    let type_off = pre_pos + marker_pre.len();
    let f_type = u32::from_le_bytes(stripped[type_off..type_off + 4].try_into().unwrap());
    eprintln!("statfs returned f_type = 0x{f_type:08X}");
    // TMPFS_MAGIC = 0x01021994 (defined in linux/magic.h)
    assert_eq!(
        f_type,
        0x0102_1994,
        "expected f_type = TMPFS_MAGIC (0x01021994), got 0x{f_type:08X}; \
         if it's a different magic, the rootfs is something other than tmpfs; \
         if 0, statfs returned -errno or didn't fill the struct; {}",
        dump_uart_on_failure(&cumulative, "statfs-wrong")
    );
}

/// `sys_access` milestone: /init creates `/probe`, accesses it
/// (expect 0), unlinks, accesses again (expect -ENOENT), prints
/// both returns. Pairs the success and failure return paths of
/// the path-based existence check.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_access_milestone() {
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
    let cpio = build_initramfs_access();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE PRE=";
    let marker_mid: &[u8] = b"POST=";
    let marker_end_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_end_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "access")
            )
        });
    eprintln!("access milestone end marker after {steps} steps");

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
    let pre_off = pre_pos + marker_pre.len();
    let pre = u32::from_le_bytes(stripped[pre_off..pre_off + 4].try_into().unwrap());

    let mid_pos = stripped
        .windows(marker_mid.len())
        .position(|w| w == marker_mid)
        .expect("marker_mid");
    let post_off = mid_pos + marker_mid.len();
    let post = u32::from_le_bytes(stripped[post_off..post_off + 4].try_into().unwrap());

    eprintln!(
        "access pre = 0x{pre:08X} ({}), post = 0x{post:08X} ({})",
        pre as i32, post as i32
    );
    assert_eq!(
        pre,
        0,
        "expected access(/probe, F_OK) = 0 (file exists), got 0x{pre:08X}; {}",
        dump_uart_on_failure(&cumulative, "access-pre-nonzero")
    );
    assert_eq!(
        post,
        0xFFFF_FFFE,
        "expected access after unlink = -ENOENT (0xFFFFFFFE), got 0x{post:08X}; \
         if it equals 0, unlink failed and the file still exists; {}",
        dump_uart_on_failure(&cumulative, "access-post-wrong")
    );
}

/// `sys_getppid` in forked child: /init forks; child calls
/// `sys_getppid` (syscall 64) which should return /init's PID
/// (equal to 1). Test asserts the 4-byte value between markers
/// equals 1 — pins the kernel's parent-child link in task struct
/// via the `real_parent` field at fork time.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_getppid_in_child_milestone() {
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
    let cpio = build_initramfs_getppid_in_child();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE PARENT_PID=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "getppid_in_child")
            )
        });
    eprintln!("getppid milestone end marker after {steps} steps");

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
        .expect("marker_pre must precede marker_post");
    let ppid_off = pre_pos + marker_pre.len();
    let ppid_bytes: [u8; 4] = stripped[ppid_off..ppid_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let ppid = u32::from_le_bytes(ppid_bytes);
    eprintln!("child's getppid returned ppid = {ppid}");
    assert_eq!(
        ppid,
        1,
        "expected child's getppid = 1 (parent is /init = PID 1), got {ppid}; {}",
        dump_uart_on_failure(&cumulative, "getppid-wrong")
    );
}

/// `sys_chmod` milestone: /init creates `/probe` with mode 0o644,
/// chmod's to 0o600, stats it, reads `st_mode`. Test asserts the
/// mode equals `S_IFREG | 0o600 = 0x81C0`.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_chmod_milestone() {
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
    let cpio = build_initramfs_chmod();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE MODE=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "chmod")
            )
        });
    eprintln!("chmod milestone end marker after {steps} steps");

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
        .expect("marker_pre must precede marker_post");
    let mode_off = pre_pos + marker_pre.len();
    let mode_bytes: [u8; 4] = stripped[mode_off..mode_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let st_mode = u32::from_le_bytes(mode_bytes);
    eprintln!("stat after chmod returned st_mode = 0o{st_mode:o} (0x{st_mode:04X})");
    // S_IFREG = 0o100000 = 0x8000; 0o600 = 0x180
    // Combined: 0o100600 = 0x81C0
    assert_eq!(
        st_mode,
        0o100_600,
        "expected st_mode = 0o100600 (S_IFREG | 0o600), got 0o{st_mode:o}; \
         if it equals 0o100644, chmod didn't change the mode; {}",
        dump_uart_on_failure(&cumulative, "chmod-wrong")
    );
}

/// `sys_truncate` milestone: /init writes a 10-byte file, calls
/// `sys_truncate(path, 4)`, then `sys_stat64` and reads
/// `st_size`. Test asserts the value equals 4 — proves the
/// kernel actually shrank the file. Different from the stat
/// milestone which only proves write extends `i_size`; this one
/// proves truncate shrinks it.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_truncate_milestone() {
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
    let cpio = build_initramfs_truncate();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE TRUNC_SIZE=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "truncate")
            )
        });
    eprintln!("truncate milestone end marker after {steps} steps");

    // Binary u32 may contain 0x0A which kernel ONLCRs — strip
    // `\r\n` → `\n` before extraction (same pattern as nanosleep).
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
        .expect("marker_pre must precede marker_post");
    let size_off = pre_pos + marker_pre.len();
    let size_bytes: [u8; 4] = stripped[size_off..size_off + 4]
        .try_into()
        .expect("4 bytes between markers");
    let st_size = u32::from_le_bytes(size_bytes);
    eprintln!("stat after truncate returned st_size = {st_size}");
    assert_eq!(
        st_size,
        4,
        "expected st_size = 4 (after truncating 10-byte file to 4), got {st_size}; \
         if it equals 10, truncate didn't shrink; {}",
        dump_uart_on_failure(&cumulative, "truncate-wrong")
    );
}

/// `sys_rename` milestone: /init creates `/a` with "RENDATA",
/// renames to `/b`, opens `/b`, reads the content, prints between
/// markers. Test asserts the round-tripped 7 bytes equal
/// `b"RENDATA"`. Pins kernel's dentry-tree rename path —
/// inode moves with the dentry, content stays intact.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_rename_milestone() {
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
    let cpio = build_initramfs_rename();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let marker_pre: &[u8] = b"[USERSPACE RENAMED=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps — rename may have failed \
                 and the subsequent open(/b) returned -ENOENT; {}",
                dump_uart_on_failure(&cumulative, "rename")
            )
        });
    eprintln!("rename milestone end marker after {steps} steps");

    let pre_pos = cumulative
        .windows(marker_pre.len())
        .position(|w| w == marker_pre)
        .expect("marker_pre must precede marker_post");
    let buf_start = pre_pos + marker_pre.len();
    let rename_bytes: [u8; 7] = cumulative[buf_start..buf_start + 7]
        .try_into()
        .expect("7 bytes between markers");
    eprintln!(
        "rename round-trip via /b: {:?}",
        std::str::from_utf8(&rename_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        &rename_bytes,
        b"RENDATA",
        "expected content via /b = b\"RENDATA\", got {:02X?} — if all zeros, rename \
         likely failed and the second open got -ENOENT; {}",
        rename_bytes,
        dump_uart_on_failure(&cumulative, "rename-wrong")
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
/// `sys_chdir` + `sys_getcwd` milestone: /init makes `/sub`,
/// chdir's into it, then reads the cwd back via `sys_getcwd`.
/// Test asserts mkdir==0, chdir==0, getcwd==5 (len of "/sub\0"),
/// and the cwd buffer starts with "/sub".
///
/// History: this was originally a diagnostic (`chdir_diag`) for a
/// supposed "getcwd returns 0" blocker. Root-caused 2026-05-29 as
/// a FALSE ALARM — getcwd works fine (returns 5, fills "/sub").
/// The original "0" reading was a test-harness bug (wrong buffer
/// offset); a second false reading came from searching for "CWD="
/// which collides with the "GETCWD=" marker substring. With a
/// distinct "PWD=" marker the chain reads cleanly and the
/// milestone passes — so this is now a real production milestone
/// and the getcwd blocker is removed from the README.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_chdir_milestone() {
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

    let m_mkdir: &[u8] = b"[USERSPACE MKDIR=";
    let m_chdir: &[u8] = b"CHDIR=";
    let m_getcwd: &[u8] = b"GETCWD=";
    let m_cwd: &[u8] = b"PWD=";
    let marker_post_search: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let steps = run_until_marker(&mut vm, marker_post_search, 16_000_000_000, &mut cumulative)
        .unwrap_or_else(|()| {
            panic!(
                "`[USERSPACE END]` not seen in 16 B steps; {}",
                dump_uart_on_failure(&cumulative, "chdir")
            )
        });
    eprintln!("chdir diag end marker after {steps} steps");

    // All four labelled values are binary u32/strings; strip ONLCR
    // before decoding the three 4-byte returns.
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
    let read_u32 = |marker: &[u8]| -> u32 {
        let pos = stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap_or_else(|| panic!("marker {:?} not found", std::str::from_utf8(marker)));
        let off = pos + marker.len();
        u32::from_le_bytes(stripped[off..off + 4].try_into().unwrap())
    };
    let mkdir_ret = read_u32(m_mkdir);
    let chdir_ret = read_u32(m_chdir);
    let getcwd_ret = read_u32(m_getcwd);
    let cwd_pos = stripped
        .windows(m_cwd.len())
        .position(|w| w == m_cwd)
        .expect("CWD marker");
    let cwd_off = cwd_pos + m_cwd.len();
    let cwd_bytes: [u8; 8] = stripped[cwd_off..cwd_off + 8].try_into().unwrap();

    eprintln!("=== chdir + getcwd milestone ===");
    eprintln!("  sys_mkdir(\"/sub\")  ret = {}", mkdir_ret as i32);
    eprintln!("  sys_chdir(\"/sub\")  ret = {}", chdir_ret as i32);
    eprintln!("  sys_getcwd(buf,32) ret = {}", getcwd_ret as i32);
    eprintln!(
        "  cwd buf[0..8] = {:?}",
        std::str::from_utf8(&cwd_bytes).unwrap_or("<non-utf8>")
    );
    assert_eq!(
        mkdir_ret,
        0,
        "sys_mkdir(\"/sub\") returned {} (expected 0); {}",
        mkdir_ret as i32,
        dump_uart_on_failure(&cumulative, "chdir-mkdir")
    );
    assert_eq!(
        chdir_ret,
        0,
        "sys_chdir(\"/sub\") returned {} (expected 0); {}",
        chdir_ret as i32,
        dump_uart_on_failure(&cumulative, "chdir-chdir")
    );
    assert_eq!(
        getcwd_ret,
        5,
        "sys_getcwd returned {} (expected 5 = len of \"/sub\\0\"); {}",
        getcwd_ret as i32,
        dump_uart_on_failure(&cumulative, "chdir-getcwd-ret")
    );
    assert_eq!(
        &cwd_bytes[..4],
        b"/sub",
        "expected cwd to start with \"/sub\", got {:02X?}; {}",
        &cwd_bytes[..4],
        dump_uart_on_failure(&cumulative, "chdir-cwd")
    );
}

/// Diagnostic test for the pipe round-trip: runs
/// `build_initramfs_pipe_rt` which reports pipe2 ret, both fds,
/// write ret, read ret, and the round-tripped buffer. Emits the
/// pre-read values BEFORE the (possibly blocking) read, and uses
/// a bounded 6 B step budget, so a blocked read still leaves the
/// decisive P2/F0/F1/WR data in UART and fails in ~2 min not ~9.
/// ALWAYS PASSES (or skips) — it logs for analysis; promotion to
/// an asserting milestone happens once the data confirms the
/// round-trip works.
#[test]
#[ignore = "diagnostic for pipe round-trip; logs all syscall returns, ~bounded"]
fn linux_userspace_pipe_rt_diag() {
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
    let cpio = build_initramfs_pipe_rt();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let m_p2: &[u8] = b"[USERSPACE P2=";
    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    // Bounded budget: userspace is reached ~2.1 B; 6 B leaves
    // headroom for the syscalls but bounds a blocked read.
    let reached = run_until_marker(&mut vm, end, 6_000_000_000, &mut cumulative).is_ok();

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
    let read_at = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| {
                let off = p + marker.len();
                u32::from_le_bytes(stripped[off..off + 4].try_into().unwrap())
            })
    };
    // Ground-truth hex dump of the marker region — resolves any
    // ambiguity about what bytes actually landed where.
    if let Some(p) = stripped.windows(14).position(|w| w == b"[USERSPACE P2=") {
        let endp = (p + 96).min(stripped.len());
        eprintln!(
            "  raw stripped[P2..+{}] = {:02X?}",
            endp - p,
            &stripped[p..endp]
        );
        eprintln!(
            "  raw stripped[P2..] as str = {:?}",
            String::from_utf8_lossy(&stripped[p..endp])
        );
    }
    eprintln!("=== pipe round-trip diagnostic (reached_end={reached}) ===");
    eprintln!(
        "  pipe2 ret = {:?}",
        read_at(b"[USERSPACE P2=").map(|v| v as i32)
    );
    eprintln!(
        "  fds[0] (read end)  = {:?}",
        read_at(b"F0=").map(|v| v as i32)
    );
    eprintln!(
        "  fds[1] (write end) = {:?}",
        read_at(b"F1=").map(|v| v as i32)
    );
    eprintln!("  write ret = {:?}", read_at(b"WR=").map(|v| v as i32));
    eprintln!("  read ret  = {:?}", read_at(b"RD=").map(|v| v as i32));
    if let Some(bp) = stripped.windows(4).position(|w| w == b"BUF=") {
        let off = bp + 4;
        if off + 4 <= stripped.len() {
            eprintln!(
                "  buf = {:?}",
                std::str::from_utf8(&stripped[off..off + 4]).unwrap_or("<non-utf8>")
            );
        }
    }
    let _ = m_p2;
    if !reached {
        eprintln!("  → read BLOCKED (pipe empty) — write didn't reach the pipe write end");
    }
}

/// Milestone: a full anonymous-pipe round-trip in userspace.
///
/// `/init` does `pipe2(&fds, 0)`, `write(fds[1], "PIPE", 4)`,
/// `read(fds[0], buf, 4)`, and emits every syscall return plus the
/// fd pair and the round-tripped buffer over the UART. This proves
/// the whole IPC chain end-to-end:
///   * `sys_pipe2` allocates a pipe and `copy_to_user`s the fd pair,
///   * `sys_write` queues bytes into the pipe buffer,
///   * `sys_read` drains them back and `copy_to_user`s the payload.
///
/// This milestone is the regression guard for the REP-string
/// page-fault rollback fix in the CPU (`crates/cpu/src/lib.rs`):
/// before that fix, `copy_to_user`'s `rep movsl` ran its ECX count
/// to zero against physical 0 on the first write to a fresh COW
/// user page, so the EIP-rewind retry found ECX==0 and copied
/// nothing — `pipe2` returned 0 but left `fds` as `[0, 0]`, and the
/// round-trip silently failed. With the fix, the faulting `rep`
/// rolls ESI/EDI/ECX back and re-runs after `do_wp_page`, landing
/// the real data. See the blocker note in README.md ("anonymous
/// pipe round-trip", marked FIXED).
#[test]
#[ignore = "boots a real Linux bzImage; ~55s wall-clock"]
fn linux_userspace_pipe_milestone() {
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
    let cpio = build_initramfs_pipe_rt();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    let reached = run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).is_ok();
    assert!(
        reached,
        "did not reach the end marker — the read likely blocked, meaning \
         the write never reached the pipe (copy_to_user/pipe regression)"
    );

    // Strip ONLCR (\r\n -> \n) so the binary u32 values the kernel
    // TTY would otherwise have mangled survive intact.
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
    let read_at = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| {
                let off = p + marker.len();
                u32::from_le_bytes(stripped[off..off + 4].try_into().unwrap())
            })
    };

    let p2 = read_at(b"[USERSPACE P2=").map(|v| v as i32);
    let f0 = read_at(b"F0=").map(|v| v as i32);
    let f1 = read_at(b"F1=").map(|v| v as i32);
    let wr = read_at(b"WR=").map(|v| v as i32);
    let rd = read_at(b"RD=").map(|v| v as i32);
    let buf = stripped
        .windows(4)
        .position(|w| w == b"BUF=")
        .and_then(|bp| stripped.get(bp + 4..bp + 8))
        .map(|s| s.to_vec());

    assert_eq!(p2, Some(0), "pipe2 should return 0; got {p2:?}");
    // /init inherits fds 0/1/2 (stdin/stdout/stderr), so pipe2's
    // freshly-allocated read/write ends are the next two: 3 and 4.
    let f0 = f0.expect("F0 marker missing");
    let f1 = f1.expect("F1 marker missing");
    assert!(
        f0 >= 3 && f1 == f0 + 1,
        "fds should be a consecutive pair >= 3 (read end then write end); \
         got fds[0]={f0}, fds[1]={f1}"
    );
    assert_eq!(
        wr,
        Some(4),
        "write(fds[1], \"PIPE\", 4) should return 4; got {wr:?}"
    );
    assert_eq!(
        rd,
        Some(4),
        "read(fds[0], buf, 4) should return 4; got {rd:?}"
    );
    assert_eq!(
        buf.as_deref(),
        Some(&b"PIPE"[..]),
        "the bytes read back from the pipe should be \"PIPE\"; got {:?}",
        buf.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    );
}

/// Milestone: `sys_poll` readiness on a pipe. `/init` makes a pipe,
/// writes one byte into it, then polls the read end for POLLIN with
/// a zero timeout and reports `poll`'s return and the raw pollfd
/// struct. Asserts `poll` returns 1 and `revents & POLLIN` is set.
/// Pins the poll syscall + pollfd copy-in/copy-out + the kernel's
/// pipe readiness reporting — the foundation of every event loop
/// and shell `select`/`poll`.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_poll_pipe_milestone() {
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
    let cpio = build_initramfs_poll_pipe();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — pipe/poll likely failed; {}",
            dump_uart_on_failure(&cumulative, "poll-pipe")
        )
    });

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
    let find_after = |marker: &[u8]| -> Option<usize> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .map(|p| p + marker.len())
    };

    let poll_ret = find_after(b"[USERSPACE POLL=")
        .and_then(|o| stripped.get(o..o + 4))
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as i32)
        .unwrap_or_else(|| {
            panic!(
                "POLL marker not found; {}",
                dump_uart_on_failure(&cumulative, "poll-marker")
            )
        });
    let pfd_off = find_after(b"PFD=").unwrap_or_else(|| {
        panic!(
            "PFD marker not found; {}",
            dump_uart_on_failure(&cumulative, "pfd-marker")
        )
    });
    let pfd = stripped
        .get(pfd_off..pfd_off + 8)
        .expect("8 bytes of pollfd after PFD=");
    let fd = i32::from_le_bytes(pfd[0..4].try_into().unwrap());
    let events = u16::from_le_bytes(pfd[4..6].try_into().unwrap());
    let revents = u16::from_le_bytes(pfd[6..8].try_into().unwrap());
    eprintln!("poll ret={poll_ret}, pollfd{{fd={fd}, events={events:#x}, revents={revents:#x}}}");

    const POLLIN: u16 = 0x0001;
    assert_eq!(
        poll_ret,
        1,
        "poll should report exactly 1 ready fd (the readable pipe), got {poll_ret}; {}",
        dump_uart_on_failure(&cumulative, "poll-ret")
    );
    assert_eq!(
        events, POLLIN,
        "events should be unchanged (POLLIN); got {events:#x}"
    );
    assert!(
        revents & POLLIN != 0,
        "revents should have POLLIN set (the pipe has a buffered byte), got {revents:#x}; {}",
        dump_uart_on_failure(&cumulative, "poll-revents")
    );
}

/// Milestone: a real `cmd1 | cmd2` shell pipeline. /init pipes a
/// forked writer child's stdout into the parent reader's stdin via
/// `dup2`, and the parent reports the bytes it read back from the
/// pipe. Asserts the parent received `"PIPED"` — i.e. data flowed
/// writer-stdout → pipe → reader-stdin across the process boundary,
/// the exact fd-plumbing a shell does for `echo PIPED | cat`. This
/// is the capstone integration of pipe + fork + dup2 + close +
/// cross-process blocking I/O + waitpid.
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~52s wall-clock"]
fn linux_userspace_pipeline_milestone() {
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
    let cpio = build_initramfs_pipeline();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    run_until_marker(&mut vm, end, 16_000_000_000, &mut cumulative).unwrap_or_else(|()| {
        panic!(
            "`[USERSPACE END]` not seen in 16 B steps — the pipeline likely \
             deadlocked or a step failed; {}",
            dump_uart_on_failure(&cumulative, "pipeline")
        )
    });

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
    let m: &[u8] = b"[USERSPACE PIPELINE=";
    let got = stripped
        .windows(m.len())
        .position(|w| w == m)
        .and_then(|p| stripped.get(p + m.len()..p + m.len() + 5))
        .map(|s| s.to_vec());
    eprintln!(
        "pipeline received: {:?}",
        got.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    );
    assert_eq!(
        got.as_deref(),
        Some(&b"PIPED"[..]),
        "parent should have read \"PIPED\" from the pipe (writer's stdout → \
         reader's stdin); got {:?}; {}",
        got.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        dump_uart_on_failure(&cumulative, "pipeline-bytes")
    );
}

/// Milestone: the stdin-read delivery path. `/init` emits a
/// "blocking on read" marker, calls `read(0, buf, 4)` (which blocks
/// until the host injects input), then emits the read's return
/// value and the bytes that landed in `buf`. The test waits for the
/// block marker, `send_input`s a line, and asserts the bytes arrive.
///
/// This is the symmetric counterpart to `linux_userspace_pipe_milestone`
/// and the second regression guard for the REP-string page-fault
/// rollback fix (`crates/cpu/src/lib.rs`): `read`'s `copy_to_user`
/// writes the input into `buf`, which sits on a fresh demand-zero
/// page, so the first write faults exactly like `pipe2`'s fd-pair
/// copy did. Before the fix this delivered nothing — the byte was
/// echoed by the TTY (so the UART rx → ISR → ldisc path worked) but
/// `buf` stayed zero-initialised. See the README blocker note
/// ("sys_read(stdin) delivery", marked FIXED).
#[test]
#[ignore = "boots a real Linux bzImage + injects input; ~55s wall-clock"]
fn linux_userspace_read_stdin_milestone() {
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
    let cpio = build_initramfs_read_stdin();
    vm.set_ramdisk(&cpio).expect("set_ramdisk");
    vm.start_protected_mode_at(bz.code32_start);

    let q: &[u8] = b"[USERSPACE Q]\r\n";
    let end: &[u8] = b"][USERSPACE END]\r\n";
    let mut cumulative = Vec::<u8>::new();
    // Stage 1: run until /init signals it's about to block in read.
    let blocked = run_until_marker(&mut vm, q, 6_000_000_000, &mut cumulative).is_ok();
    eprintln!("=== stdin-read milestone ===");
    eprintln!("  reached read-block marker = {blocked}");
    assert!(
        blocked,
        "/init never reached the read-block marker — it didn't get to the \
         read syscall"
    );
    // Stage 2: inject a 4-printable-char line. Canonical /dev/console
    // returns the line on the newline; read(buf, 4) takes "KICK".
    vm.send_input(b"KICK\n");
    let reached_end = run_until_marker(&mut vm, end, 6_000_000_000, &mut cumulative).is_ok();

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
    let read_u32 = |marker: &[u8]| -> Option<u32> {
        stripped
            .windows(marker.len())
            .position(|w| w == marker)
            .and_then(|p| stripped.get(p + marker.len()..p + marker.len() + 4))
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    };
    eprintln!("  reached end marker = {reached_end}");
    eprintln!(
        "  read() ret (RC=) = {:?}",
        read_u32(b"RC=").map(|v| v as i32)
    );
    if let Some(p) = stripped.windows(4).position(|w| w == b"GOT=") {
        if let Some(s) = stripped.get(p + 4..p + 8) {
            eprintln!("  buf (GOT=) bytes = {:02X?}", s);
            eprintln!("  buf (GOT=) str   = {:?}", String::from_utf8_lossy(s));
        }
    }
    if let Some(p) = stripped.windows(13).position(|w| w == b"[USERSPACE Q]") {
        let endp = (p + 80).min(stripped.len());
        eprintln!(
            "  raw stripped[Q..] str = {:?}",
            String::from_utf8_lossy(&stripped[p..endp])
        );
    }

    assert!(
        reached_end,
        "did not reach the end marker after injecting input — read() never \
         returned (input not delivered to the blocked reader)"
    );
    let rc = read_u32(b"RC=").map(|v| v as i32);
    assert_eq!(
        rc,
        Some(4),
        "read(0, buf, 4) should return 4 after \"KICK\\n\" was injected; got {rc:?}"
    );
    let got = stripped
        .windows(4)
        .position(|w| w == b"GOT=")
        .and_then(|p| stripped.get(p + 4..p + 8))
        .map(|s| s.to_vec());
    assert_eq!(
        got.as_deref(),
        Some(&b"KICK"[..]),
        "the injected bytes should land in the user buffer; got {:?} \
         (all-zero here is the copy_to_user-drops-on-fresh-page regression)",
        got.as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    );
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
