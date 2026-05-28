//! End-to-end Linux 6.12 i386 boot-to-userspace milestone, captured
//! as a regression test. Mirrors the recipe documented in the
//! README's "Загрузка Linux 6.12" section:
//!
//!     WWWVM_KERNEL=/tmp/wwwvm-linux/vmlinuz \
//!     cargo test --release -- --ignored linux_userspace_milestone
//!
//! The test is `#[ignore]` because it depends on a vmlinuz file
//! we don't ship (Tinycore Core ISO `boot/vmlinuz`, 5.85 MB). One
//! run is ~95 seconds wall-clock — the test bails the moment
//! HELLO shows up, vs. the linux_boot example which intentionally
//! runs the full 16 B-step budget for diagnostics and clocks
//! ~10 min. Even at 95 seconds, this isn't something to put in
//! the default sweep.
//!
//! What it locks in: the kernel runs all the way through
//! `driver_init` + `do_initcalls`, mounts our minimal initramfs,
//! exec's PID 1 = /init, and /init's `write(1, "HELLO FROM
//! USERSPACE\n", 21)` reaches the host UART tx_buffer via the full
//! syscall path (cross-ring + tty_write + THRE IRQ).

use wwwvm_vm::Vm;

/// Build the same minimal newc cpio archive the linux_boot example
/// uses for hello mode: /init + /dev + /dev/console (S_IFCHR 5:1).
/// Inlined here so the test stays self-contained (no example
/// dependency from a `tests/` integration file).
fn build_initramfs_hello() -> Vec<u8> {
    const LOAD_ADDR: u32 = 0x0804_8000;
    const ELF_HEADER_LEN: u32 = 52;
    const PHDR_LEN: u32 = 32;
    const ENTRY_OFFSET: u32 = ELF_HEADER_LEN + PHDR_LEN;
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
    let msg_addr = LOAD_ADDR + ENTRY_OFFSET + code_len;
    let mut body = build_code(msg_addr);
    body.extend_from_slice(msg);
    let filesz = ELF_HEADER_LEN + PHDR_LEN + body.len() as u32;

    let mut elf: Vec<u8> = Vec::with_capacity(52);
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1, 0]);
    elf.extend_from_slice(&[0u8; 8]);
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&3u16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&(LOAD_ADDR + ENTRY_OFFSET).to_le_bytes());
    elf.extend_from_slice(&ELF_HEADER_LEN.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&(ELF_HEADER_LEN as u16).to_le_bytes());
    elf.extend_from_slice(&(PHDR_LEN as u16).to_le_bytes());
    elf.extend_from_slice(&1u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());

    let mut phdr: Vec<u8> = Vec::with_capacity(32);
    phdr.extend_from_slice(&1u32.to_le_bytes());
    phdr.extend_from_slice(&0u32.to_le_bytes());
    phdr.extend_from_slice(&LOAD_ADDR.to_le_bytes());
    phdr.extend_from_slice(&LOAD_ADDR.to_le_bytes());
    phdr.extend_from_slice(&filesz.to_le_bytes());
    phdr.extend_from_slice(&filesz.to_le_bytes());
    phdr.extend_from_slice(&5u32.to_le_bytes());
    phdr.extend_from_slice(&0x1000u32.to_le_bytes());

    let mut binary = elf;
    binary.extend_from_slice(&phdr);
    binary.extend_from_slice(&body);

    fn cpio(name: &str, data: &[u8], mode: u32, rdevmaj: u32, rdevmin: u32) -> Vec<u8> {
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
    let mut archive = Vec::new();
    archive.extend_from_slice(&cpio("init", &binary, 0o100_755, 0, 0));
    archive.extend_from_slice(&cpio("dev", &[], 0o040_755, 0, 0));
    archive.extend_from_slice(&cpio("dev/console", &[], 0o020_600, 5, 1));
    archive.extend_from_slice(&cpio("TRAILER!!!", &[], 0, 0, 0));
    while archive.len() & 511 != 0 {
        archive.push(0);
    }
    archive
}

/// Drive the boot for up to `STEP_BUDGET` instructions, draining
/// UART output in 100M-step chunks, and look for the literal
/// "HELLO FROM USERSPACE" anywhere in the cumulative output.
/// Returns Ok(()) on first hit, Err with the cumulative output
/// on budget exhaustion.
fn run_until_hello(vm: &mut Vm) -> Result<u64, String> {
    const STEP_BUDGET: u64 = 16_000_000_000;
    let chunk = 10_000_000u32;
    let mut steps = 0u64;
    let mut cumulative = Vec::<u8>::new();
    while steps < STEP_BUDGET {
        let (s, _) = vm.run_steps_idle_aware(chunk);
        steps += s as u64;
        if steps % 100_000_000 < chunk as u64 {
            let out = vm.drain_output();
            if !out.is_empty() {
                cumulative.extend_from_slice(&out);
                if cumulative.windows(20).any(|w| w == b"HELLO FROM USERSPACE") {
                    return Ok(steps);
                }
            }
        }
    }
    let cumulative_str = String::from_utf8_lossy(&cumulative).into_owned();
    Err(cumulative_str)
}

/// Full Linux 6.12 boot to userspace. Skipped if the kernel file
/// isn't present (so contributors without the binary can still
/// run `cargo test -- --ignored`).
#[test]
#[ignore = "requires /tmp/wwwvm-linux/vmlinuz; ~95s wall-clock"]
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

    match run_until_hello(&mut vm) {
        Ok(steps) => {
            eprintln!("HELLO FROM USERSPACE found after {steps} steps");
        }
        Err(cumulative) => {
            // Dump the last 1 KiB so the failure is debuggable.
            let tail_start = cumulative.len().saturating_sub(1024);
            panic!(
                "HELLO FROM USERSPACE not seen in 16 B steps; \
                 last 1 KiB of UART output:\n{}",
                &cumulative[tail_start..]
            );
        }
    }
}
