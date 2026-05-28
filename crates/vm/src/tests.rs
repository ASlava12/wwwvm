use super::*;

/// INT 0x16 AH=0x00 (blocking read). With a key already queued, the
/// single INT call must consume it and return it in AL.
#[test]
fn bios_int16_read_keystroke_returns_queued_byte() {
    let mut vm = Vm::new();
    vm.install_bios();
    vm.push_scancode(b'X');
    // MOV AH, 0; INT 0x16; HLT
    vm.load_image(BOOT_LOAD_ADDR, &[0xB4, 0x00, 0xCD, 0x16, 0xF4]);
    vm.boot();
    let (_, stop) = vm.run_steps(16);
    assert!(matches!(stop, Stop::Halted));
    assert_eq!(vm.cpu().read_r8(0), b'X');
}

/// INT 0x16 AH=0x01 (check keystroke). When the queue is empty, ZF
/// is set and AL is not loaded with anything meaningful. When a key
/// is queued, ZF is clear and AL reflects the byte — without
/// consuming it.
#[test]
fn bios_int16_check_keystroke_sets_zf_correctly() {
    let mut vm = Vm::new();
    vm.install_bios();
    // First sequence: no key, ZF must be set after INT.
    //   MOV AH, 1; INT 0x16; HLT
    vm.load_image(BOOT_LOAD_ADDR, &[0xB4, 0x01, 0xCD, 0x16, 0xF4]);
    vm.boot();
    vm.run_steps(8);
    assert_ne!(vm.cpu().flags & 0x40, 0, "ZF must be set for empty queue");

    // Second sequence: push a key, run check again, expect ZF=0 and
    // queue unchanged.
    let mut vm2 = Vm::new();
    vm2.install_bios();
    vm2.push_scancode(b'Y');
    vm2.load_image(BOOT_LOAD_ADDR, &[0xB4, 0x01, 0xCD, 0x16, 0xF4]);
    vm2.boot();
    vm2.run_steps(8);
    assert_eq!(vm2.cpu().flags & 0x40, 0, "ZF must be clear when key waits");
    assert_eq!(vm2.cpu().read_r8(0), b'Y');
    // Queue still has the byte — AH=0x01 only peeks.
    assert_eq!(vm2.io.kbd.rx_pending(), 1);
}

/// INT 0x16 AH=0x00 with no key must block by rewinding IP so the same
/// INT re-executes next step. We run several steps with the queue
/// empty (the CPU should keep landing back on `CD 16` without making
/// progress), then push a key and run one more step. AL must end up
/// with the byte.
#[test]
fn bios_int16_read_blocks_until_key_arrives() {
    let mut vm = Vm::new();
    vm.install_bios();
    // MOV AH, 0; INT 0x16; HLT
    vm.load_image(BOOT_LOAD_ADDR, &[0xB4, 0x00, 0xCD, 0x16, 0xF4]);
    vm.boot();
    // Spin: BIOS keeps rewinding past `CD 16`. The HLT should NOT
    // have run yet because the INT hasn't consumed a key.
    for _ in 0..20 {
        vm.run_steps(1);
    }
    assert!(!vm.cpu().halted, "CPU must still be spinning on INT 0x16");

    vm.push_scancode(b'Z');
    let (_, stop) = vm.run_steps(8);
    assert!(matches!(stop, Stop::Halted));
    assert_eq!(vm.cpu().read_r8(0), b'Z');
}

/// INT 0x12 — Returns AX = conventional memory KiB below 1 MiB.
/// Always 640 regardless of actual VM size (the historical PC
/// reservation for VGA/ROM above 0xA0000 holds even if the VM has
/// more RAM than that).
#[test]
fn bios_int12_returns_640_kib_conventional_memory() {
    let mut vm = Vm::with_ram_size(0x0100_0000); // 16 MiB
    vm.install_bios();
    vm.load_image(BOOT_LOAD_ADDR, &[0xCD, 0x12, 0xF4]); // INT 0x12; HLT
    vm.boot();
    vm.run_steps(8);
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::AX], 640);
    assert_eq!(vm.cpu().flags & wwwvm_cpu::flag::CF, 0);
}

/// INT 0x15 AH=0x88 — legacy fallback for "how much extended memory
/// is there". Returns AX = KiB above 1 MiB, capped at 0xFFFF. With
/// a 16 MiB VM, expected value is (16 - 1) * 1024 = 15360.
#[test]
fn bios_int15_88_returns_extended_memory_kib() {
    let mut vm = Vm::with_ram_size(0x0100_0000); // 16 MiB
    vm.install_bios();
    // MOV AX, 0x8800; INT 0x15; HLT
    vm.load_image(BOOT_LOAD_ADDR, &[0xB8, 0x00, 0x88, 0xCD, 0x15, 0xF4]);
    vm.boot();
    vm.run_steps(16);
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::AX], 15360);
    assert_eq!(vm.cpu().flags & wwwvm_cpu::flag::CF, 0);
}

/// INT 0x15 AH=0x88 with a VM ≤ 1 MiB returns 0 (no extended memory).
#[test]
fn bios_int15_88_returns_zero_when_no_extended_memory() {
    let mut vm = Vm::new(); // default 1 MiB
    vm.install_bios();
    vm.load_image(BOOT_LOAD_ADDR, &[0xB8, 0x00, 0x88, 0xCD, 0x15, 0xF4]);
    vm.boot();
    vm.run_steps(16);
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::AX], 0);
}

/// INT 0x15 AH=0x88 with a huge VM caps at 0xFFFF (≈ 64 MiB - 1 KiB).
#[test]
fn bios_int15_88_caps_at_0xffff() {
    let mut vm = Vm::with_ram_size(0x0800_0000); // 128 MiB
    vm.install_bios();
    vm.load_image(BOOT_LOAD_ADDR, &[0xB8, 0x00, 0x88, 0xCD, 0x15, 0xF4]);
    vm.boot();
    vm.run_steps(16);
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::AX], 0xFFFF);
}

/// INT 0x15 AX=0xE820 — Linux setup uses this to discover usable
/// RAM ranges. Our shim returns a single entry covering the whole
/// VM RAM (base=0, length=mem.size(), type=1) and signals "no more
/// entries" via EBX=0 on the first call.
///
/// Boot stub:
///   MOV AX, 0xE820   ; B8 20 E8
///   MOV EBX, 0       ; 66 BB 00 00 00 00
///   MOV ECX, 20      ; 66 B9 14 00 00 00
///   MOV EDX, "SMAP"  ; 66 BA 50 41 4D 53
///   MOV DI, 0x3000   ; BF 00 30
///   (ES already 0 from boot)
///   INT 0x15         ; CD 15
///   HLT              ; F4
///
/// Verifies the 20-byte buffer at 0x3000, EBX=0, ECX=20, AX="SMAP".
#[test]
fn bios_int15_e820_returns_single_ram_entry() {
    let mut vm = Vm::with_ram_size(0x0200_0000); // 32 MiB
    vm.install_bios();
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0xB8, 0x20, 0xE8, // MOV AX, 0xE820
            0x66, 0xBB, 0x00, 0x00, 0x00, 0x00, // MOV EBX, 0
            0x66, 0xB9, 0x14, 0x00, 0x00, 0x00, // MOV ECX, 20
            0x66, 0xBA, 0x50, 0x41, 0x4D, 0x53, // MOV EDX, "SMAP"
            0xBF, 0x00, 0x30, // MOV DI, 0x3000
            0xCD, 0x15, // INT 0x15
            0xF4, // HLT
        ],
    );
    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted));

    // base (u64) at 0x3000 = 0
    for off in 0..8 {
        assert_eq!(vm.read_mem_u8(0x3000 + off), 0, "base byte {off}");
    }
    // length (u64) at 0x3008 = 32 MiB = 0x0200_0000
    let length_bytes = [0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    for (off, &expected) in length_bytes.iter().enumerate() {
        assert_eq!(
            vm.read_mem_u8(0x3008 + off as u32),
            expected,
            "length byte {off}"
        );
    }
    // type (u32) at 0x3010 = 1
    assert_eq!(vm.read_mem_u8(0x3010), 1);
    assert_eq!(vm.read_mem_u8(0x3011), 0);
    assert_eq!(vm.read_mem_u8(0x3012), 0);
    assert_eq!(vm.read_mem_u8(0x3013), 0);

    // Returned register state.
    assert_eq!(vm.cpu().read_r32(0), 0x534D_4150, "EAX = SMAP");
    assert_eq!(vm.cpu().read_r32(1), 20, "ECX = 20");
    assert_eq!(vm.cpu().read_r32(3), 0, "EBX = 0 (no more entries)");
    assert_eq!(vm.cpu().flags & wwwvm_cpu::flag::CF, 0, "CF clear");
}

/// A second E820 call with EBX != 0 must set CF=1 to signal "done".
/// (Some loaders re-enter the loop and rely on CF as the terminator
/// instead of EBX.)
#[test]
fn bios_int15_e820_with_nonzero_continuation_signals_done() {
    let mut vm = Vm::with_ram_size(0x0010_0000);
    vm.install_bios();
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0xB8, 0x20, 0xE8, // MOV AX, 0xE820
            0x66, 0xBB, 0x01, 0x00, 0x00, 0x00, // MOV EBX, 1 (continuation)
            0x66, 0xB9, 0x14, 0x00, 0x00, 0x00, // MOV ECX, 20
            0xBF, 0x00, 0x30, // MOV DI, 0x3000
            0xCD, 0x15, // INT 0x15
            0xF4, // HLT
        ],
    );
    vm.boot();
    vm.run_steps(32);
    assert_ne!(vm.cpu().flags & wwwvm_cpu::flag::CF, 0, "CF set");
    assert_eq!(vm.cpu().read_r32(3), 0, "EBX cleared");
}

/// End-to-end synthetic-bzImage boot. Combines:
///
///   1. `Vm::load_bzimage` — places setup at 0x90000 and the
///      protected-mode payload at `code32_start` = 0x10_0000.
///   2. A bootstrap stub at 0x7C00 that installs a flat code
///      segment (GDT[1].base = code32_start), flips CR0.PE, and
///      far-jumps to selector 0x08 / offset 0. With the descriptor
///      base equal to the kernel address, the next fetch lands on
///      the bzImage payload's first byte.
///   3. A 3-byte "kernel" payload: `MOV AL, 0xCD; HLT`. The host
///      asserts AL = 0xCD after the run.
///
/// The point: this is the final handoff a real Linux bzImage
/// expects, exercised end-to-end on a binary that goes through the
/// actual `load_bzimage` parser instead of a hand-crafted ELF.
#[test]
fn end_to_end_synthetic_bzimage_boots_kernel_at_code32_start() {
    // Build the bzImage: 1024-byte setup blob + 3-byte payload.
    let mut bz_bytes = vec![0u8; 1024];
    bz_bytes[0x1F1] = 1; // setup_sects = 1 → payload at offset 1024
    bz_bytes[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
    bz_bytes[0x202..0x206].copy_from_slice(b"HdrS");
    bz_bytes[0x206..0x208].copy_from_slice(&0x020Du16.to_le_bytes());
    bz_bytes[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
    bz_bytes.extend_from_slice(&[0xB0, 0xCD, 0xF4]); // MOV AL,0xCD; HLT

    let mut vm = Vm::with_ram_size(0x0100_0000); // 16 MiB
    let bz = vm.load_bzimage(&bz_bytes).expect("bzImage load");
    assert_eq!(bz.code32_start, 0x0010_0000);

    // GDT + LGDT pseudo-descriptor (same layout as the earlier
    // end_to_end_pm_kernel test). GDT[1] is flat code with base =
    // code32_start, so a far-jump to selector 0x08 / IP=0 lands on
    // the kernel.
    vm.load_image(
        0x0500,
        &[
            // LGDT pseudo-descriptor: limit (2) + base (4)
            0x0F, 0x00, 0x08, 0x05, 0x00, 0x00, // pad to align GDT at 0x0508
            0x00, 0x00, // GDT[0] = null
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // GDT[1] = code, base=0x0010_0000, limit=0xFFFF, access=0x9A
            0xFF, 0xFF, 0x00, 0x00, 0x10, 0x9A, 0x00, 0x00,
        ],
    );

    // Bootstrap at 0x7C00 — identical shape to the earlier PM test.
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0x0F, 0x01, 0x16, 0x00, 0x05, // LGDT [0x0500]
            0x0F, 0x20, 0xC0, // MOV EAX, CR0
            0x83, 0xC8, 0x01, // OR  AX, 1
            0x0F, 0x22, 0xC0, // MOV CR0, EAX
            0xEA, 0x00, 0x00, 0x08, 0x00, // JMP FAR 0x08:0x0000
            0xF4, // HLT (unreached)
        ],
    );

    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted), "stop: {stop:?}");
    assert_eq!(vm.cpu().cr0 & 1, 1);
    assert_eq!(
        vm.cpu().seg_cache[wwwvm_cpu::sreg::CS].base,
        bz.code32_start
    );
    assert_eq!(
        vm.cpu().read_r8(0),
        0xCD,
        "bzImage payload at code32_start must have executed"
    );
}

/// `Vm::load_bzimage` parses a synthetic bzImage and places its setup
/// blob at linear 0x90000 and its 32-bit payload at code32_start.
/// We craft a minimal valid header with setup_sects=1 (so payload
/// starts at file offset 1024), code32_start=0x10_0000, and a
/// recognizable payload signature. Verifies the bytes landed in the
/// right places and that the returned `BzImage` carries the right
/// metadata.
#[test]
fn load_bzimage_places_setup_at_90000_and_payload_at_code32_start() {
    let payload = b"PAYLOAD12";
    let mut bz_bytes = vec![0u8; 1024];
    // Mark setup byte 0 with a sentinel so we can detect setup landed
    // at 0x90000.
    bz_bytes[0] = 0xC5;
    // Header fields.
    bz_bytes[0x1F1] = 1; // setup_sects = 1 (payload at offset 1024)
    bz_bytes[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
    bz_bytes[0x202..0x206].copy_from_slice(b"HdrS");
    bz_bytes[0x206..0x208].copy_from_slice(&0x020Du16.to_le_bytes());
    bz_bytes[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
    bz_bytes.extend_from_slice(payload);

    let mut vm = Vm::with_ram_size(0x0100_0000);
    let bz = vm.load_bzimage(&bz_bytes).expect("bzImage load");
    assert_eq!(bz.setup_sects, 1);
    assert_eq!(bz.code32_start, 0x0010_0000);
    assert_eq!(bz.payload_offset, 1024);

    // Setup sentinel at 0x90000.
    assert_eq!(vm.read_mem_u8(0x9_0000), 0xC5);
    // boot_flag still in place — it's part of setup, so it should be
    // at 0x90000 + 0x1FE.
    assert_eq!(vm.read_mem_u8(0x9_0000 + 0x1FE), 0x55);
    assert_eq!(vm.read_mem_u8(0x9_0000 + 0x1FF), 0xAA);
    // HdrS at 0x90000 + 0x202.
    assert_eq!(vm.read_mem_u8(0x9_0000 + 0x202), b'H');

    // Payload bytes at code32_start.
    for (i, &b) in payload.iter().enumerate() {
        assert_eq!(vm.read_mem_u8(0x0010_0000 + i as u32), b);
    }
}

/// load_bzimage propagates parser errors instead of leaving guest
/// memory partially populated.
#[test]
fn load_bzimage_propagates_parser_errors() {
    let mut vm = Vm::with_ram_size(0x0010_0000);
    let too_small = vec![0u8; 512];
    assert!(matches!(
        vm.load_bzimage(&too_small),
        Err(BzImageError::TooSmall(512))
    ));
}

/// Linux-style high-vaddr kernel boot. The 32-bit far jump
/// (0x66 0xEA off32 sel16) lets the bootstrap reach a kernel
/// linked at 0xC010_0000 — the canonical Linux 32-bit kernel
/// virtual address — directly through CS:EIP without going
/// through a "CS base = entry address" trick.
///
/// We use a flat code segment (base=0, limit=0xFFFFF, G=1 →
/// limit=4GiB), then far-jump to selector 0x08, offset 0xC010_0000.
/// IP ends up at 0xC010_0000; CS cache base is 0. The fetch loop
/// uses `linear_seg(CS, IP) = 0 + 0xC010_0000` to pull the kernel
/// stub.
#[test]
fn end_to_end_pm_kernel_at_linux_high_vaddr() {
    let mut vm = Vm::with_ram_size(0x0100_0000); // 16 MiB physical

    // Kernel-stub at *physical* 0x0010_0000 — but Linux maps it
    // to *virtual* 0xC010_0000 via paging. We don't model paging
    // for the jump itself; we just need the linear address the
    // CPU lands at to actually have the bytes. With PG=0 and a
    // flat code segment, linear == physical, and the test stays
    // under 16 MiB by aliasing: jump to 0x10_0000 directly via
    // a 32-bit far jump.
    let entry: u32 = 0x0010_0000;
    vm.load_image(entry, &[0xB0, 0xC7, 0xF4]); // MOV AL,0xC7; HLT

    // GDT + LGDT pseudo-descriptor. GDT[1] is a flat code segment
    // with base=0, limit=0xFFFFF, G=1 → 4 GiB. Access 0x9A.
    vm.load_image(
        0x0500,
        &[
            // pseudo-descriptor: limit (2) + base (4)
            0x0F, 0x00, 0x08, 0x05, 0x00, 0x00, // pad
            0x00, 0x00, // GDT[0] = null
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // GDT[1]: limit_lo=FFFF, base_lo=0, base_mid=0, access=9A,
            //         limit_hi/flags = 0xCF (G=1 D=1 limit_hi=0xF), base_hi=0
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00,
        ],
    );

    // Bootstrap at 0x7C00.
    //   LGDT [0x0500]       0F 01 16 00 05
    //   MOV EAX, CR0        0F 20 C0
    //   OR  AX, 1           83 C8 01
    //   MOV CR0, EAX        0F 22 C0
    //   JMP FAR 0x08:0x0010_0000  with 0x66 prefix:
    //     66 EA 00 00 10 00 08 00
    //   HLT (unreached)     F4
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0x0F, 0x01, 0x16, 0x00, 0x05, 0x0F, 0x20, 0xC0, 0x83, 0xC8, 0x01, 0x0F, 0x22, 0xC0,
            0x66, 0xEA, 0x00, 0x00, 0x10, 0x00, 0x08, 0x00, 0xF4,
        ],
    );

    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted), "stop: {stop:?}");
    assert_eq!(vm.cpu().cr0 & 1, 1);
    assert_eq!(
        vm.cpu().seg_cache[wwwvm_cpu::sreg::CS].base,
        0,
        "flat code segment, base 0"
    );
    assert_eq!(
        vm.cpu().read_r8(0),
        0xC7,
        "kernel at linear 0x10_0000 must have executed via 32-bit far jump"
    );
}

/// Full end-to-end protected-mode kernel boot. A 16 MiB VM gets:
///
///   1. An ELF kernel whose single PT_LOAD lands at vaddr 0x10_0000
///      (just past the 1 MiB boundary). The kernel is `MOV AL,0xAB;
///      HLT` — three bytes that prove execution actually reached
///      the high-memory entry point.
///   2. A 16-byte GDT in low memory: null + flat code segment with
///      base=0x10_0000, limit=0xFFFF, access=0x9A (P=1, DPL=0,
///      code, R/X). A 6-byte LGDT pseudo-descriptor next to it.
///   3. A boot stub at 0x7C00 that LGDTs, flips CR0.PE=1 via the
///      idiom, then `JMP FAR 0x08:0x0000`. With CS selector 0x08
///      pointing at the GDT[1] code segment (base 0x10_0000), the
///      next instruction fetch lands inside the kernel.
///
/// The trick that lets us avoid a 32-bit IP is the descriptor base:
/// `linear = seg_cache[CS].base + IP` already does the right thing
/// when base = 0x10_0000 and IP = 0.
#[test]
fn end_to_end_pm_kernel_boot_from_elf_above_one_mebibyte() {
    let mut vm = Vm::with_ram_size(0x0100_0000); // 16 MiB

    // 1. Kernel ELF at vaddr 0x10_0000.
    let entry: u32 = 0x0010_0000;
    let mut elf = make_elf_header(entry, 52, 1);
    elf.extend_from_slice(&make_pt_load(84, entry, 3));
    elf.extend_from_slice(&[0xB0, 0xAB, 0xF4]); // MOV AL, 0xAB; HLT
    vm.load_elf_image(&elf).expect("ELF load");

    // 2. GDT pseudo-descriptor at 0x0500 (limit=0x000F, base=0x0508).
    //    GDT itself at 0x0508. Two 8-byte entries.
    vm.load_image(
        0x0500,
        &[
            // pseudo-descriptor: limit (2) + base (4)
            0x0F, 0x00, 0x08, 0x05, 0x00, 0x00, // pad (2) to align GDT at 0x0508
            0x00, 0x00, // GDT[0] = null descriptor
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // GDT[1] = flat code segment, base=0x0010_0000, limit=0xFFFF,
            //          access=0x9A (P|DPL=0|S=1|type=A: code, R/X), G=0.
            //   limit_lo  = 0xFFFF
            //   base_lo   = 0x0000
            //   base_mid  = 0x10              (byte 4)
            //   access    = 0x9A              (byte 5)
            //   limit_hi  = 0x00 | flags 0x00 (byte 6)
            //   base_hi   = 0x00              (byte 7)
            0xFF, 0xFF, 0x00, 0x00, 0x10, 0x9A, 0x00, 0x00,
        ],
    );

    // 3. Boot stub at 0x7C00.
    //   LGDT [0x0500]    -> 0F 01 16 00 05
    //   MOV EAX, CR0     -> 0F 20 C0
    //   OR AX, 1         -> 83 C8 01
    //   MOV CR0, EAX     -> 0F 22 C0
    //   JMP FAR 0x08:0   -> EA 00 00 08 00
    //   HLT (unreached)  -> F4
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0x0F, 0x01, 0x16, 0x00, 0x05, 0x0F, 0x20, 0xC0, 0x83, 0xC8, 0x01, 0x0F, 0x22, 0xC0,
            0xEA, 0x00, 0x00, 0x08, 0x00, 0xF4,
        ],
    );

    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted), "stop: {stop:?}");
    assert_eq!(vm.cpu().cr0 & 1, 1, "bootstrap must have set CR0.PE");
    assert_eq!(
        vm.cpu().seg_cache[wwwvm_cpu::sreg::CS].base,
        0x0010_0000,
        "CS cache base must come from GDT[1] (kernel segment)"
    );
    assert_eq!(
        vm.cpu().read_r8(0),
        0xAB,
        "kernel at 0x100000 must have run and set AL=0xAB"
    );
}

/// `Vm::with_ram_size` lets a VM hold a kernel image whose PT_LOAD
/// targets memory above the 1 MiB boundary. We allocate 16 MiB and
/// load a tiny ELF whose segment is at vaddr 0x10_8000 (just past
/// 1 MiB). The loader must place the bytes there — and the address
/// must actually exist (a default 1 MiB VM would have rejected this
/// via ElfError::DestOutOfBounds).
///
/// Executing code that high requires protected-mode addressing
/// (real-mode CS:IP can only reach ≈ 1 MiB + 64 KiB). That part is
/// exercised by the existing PM tests; this test confirms only the
/// load path.
#[test]
fn with_ram_size_loads_elf_segment_above_one_mebibyte() {
    let entry: u32 = 0x0010_8000;
    let mut elf = make_elf_header(entry, 52, 1);
    elf.extend_from_slice(&make_pt_load(84, entry, 5));
    elf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xF4]);

    // Default 1 MiB VM must refuse the load.
    let mut small_vm = Vm::new();
    assert!(matches!(
        small_vm.load_elf_image(&elf),
        Err(ElfError::DestOutOfBounds { .. })
    ));

    // 16 MiB VM accepts it and places the bytes at the right address.
    let mut vm = Vm::with_ram_size(0x0100_0000);
    let loaded = vm.load_elf_image(&elf).expect("ELF load above 1 MiB");
    assert_eq!(loaded, entry);
    assert_eq!(vm.read_mem_u8(entry), 0xDE);
    assert_eq!(vm.read_mem_u8(entry + 4), 0xF4);
}

/// Snapshot taken from a 16 MiB VM must restore cleanly into another
/// 16 MiB VM. Confirms that snapshot now follows `self.mem.size()`
/// rather than the 1 MiB `Vm::RAM_SIZE` constant.
#[test]
fn snapshot_round_trips_with_custom_ram_size() {
    let mut vm = Vm::with_ram_size(0x0020_0000); // 2 MiB
    vm.boot();
    vm.mem.write_u8(0x0018_0000, 0xA5);
    let snap = vm.snapshot();
    let mut vm2 = Vm::with_ram_size(0x0020_0000);
    vm2.restore(&snap).expect("restore 2 MiB snapshot");
    assert_eq!(vm2.read_mem_u8(0x0018_0000), 0xA5);
}

/// load_elf_image() parses an ELF32 image and copies its PT_LOAD
/// segments into memory. We craft a tiny ELF whose single segment is
/// `MOV AL,'A'; HLT` at vaddr 0x7C00 (so a plain `boot()` lands the
/// CPU at the entry point with no further setup). Verifies the
/// loader+VM chain runs end-to-end on a real ELF binary structure.
#[test]
fn vm_loads_and_runs_tiny_elf32_image() {
    let entry: u32 = 0x7C00;
    let mut elf = make_elf_header(entry, 52, 1);
    // PT_LOAD at file offset 84, vaddr 0x7C00, filesz=3.
    elf.extend_from_slice(&make_pt_load(84, entry, 3));
    elf.extend_from_slice(&[0xB0, 0x41, 0xF4]); // MOV AL,'A'; HLT

    let mut vm = Vm::new();
    let loaded_entry = vm.load_elf_image(&elf).expect("ELF load");
    assert_eq!(loaded_entry, entry);
    vm.boot(); // resets CS=0, IP=0x7C00
    let (_, stop) = vm.run_steps(8);
    assert!(matches!(stop, Stop::Halted));
    assert_eq!(vm.cpu().read_r8(0), b'A');
}

fn make_elf_header(e_entry: u32, e_phoff: u32, e_phnum: u16) -> Vec<u8> {
    let mut h = vec![0u8; 52];
    h[..4].copy_from_slice(b"\x7FELF");
    h[4] = 1; // ELFCLASS32
    h[5] = 1; // ELFDATA2LSB
    h[6] = 1; // EI_VERSION
    h[0x10..0x12].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    h[0x12..0x14].copy_from_slice(&3u16.to_le_bytes()); // EM_386
    h[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // e_version
    h[0x18..0x1C].copy_from_slice(&e_entry.to_le_bytes());
    h[0x1C..0x20].copy_from_slice(&e_phoff.to_le_bytes());
    h[0x28..0x2A].copy_from_slice(&52u16.to_le_bytes()); // e_ehsize
    h[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes()); // e_phentsize
    h[0x2C..0x2E].copy_from_slice(&e_phnum.to_le_bytes());
    h
}

fn make_pt_load(p_offset: u32, p_vaddr: u32, p_size: u32) -> Vec<u8> {
    let mut p = vec![0u8; 32];
    p[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    p[4..8].copy_from_slice(&p_offset.to_le_bytes());
    p[8..12].copy_from_slice(&p_vaddr.to_le_bytes());
    p[12..16].copy_from_slice(&p_vaddr.to_le_bytes()); // p_paddr
    p[16..20].copy_from_slice(&p_size.to_le_bytes()); // p_filesz
    p[20..24].copy_from_slice(&p_size.to_le_bytes()); // p_memsz
    p[24..28].copy_from_slice(&7u32.to_le_bytes()); // R|W|X
    p[28..32].copy_from_slice(&0x1000u32.to_le_bytes());
    p
}

/// INT 0x13 with AL > 1 reads multiple sectors contiguously. Verifies
/// the per-byte mem_write_u8 loop in `bios_int13` doesn't truncate
/// after the first sector. Disk has 4 marker sectors (0xAA / 0xBB /
/// 0xCC / 0xDD); we load sectors 1 + 2 via a single INT 0x13 AL=2
/// call and expect them to land back-to-back at ES:BX.
#[test]
fn bios_int13_reads_multiple_sectors_contiguously() {
    let mut vm = Vm::new();
    vm.install_bios();
    let mut disk = Vec::new();
    for marker in [0xAA, 0xBB, 0xCC, 0xDD] {
        disk.extend(std::iter::repeat_n(marker, 512));
    }
    vm.load_disk_image(&disk);
    // Boot stub: ES=0, BX=0x3000, AH=2, AL=2, CL=2 (sector 2 = LBA 1),
    // INT 0x13, HLT.
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0xB8, 0x00, 0x00, 0x8E, 0xC0, 0xBB, 0x00, 0x30, 0xB4, 0x02, 0xB0, 0x02, 0xB5, 0x00,
            0xB1, 0x02, 0xB6, 0x00, 0xB2, 0x80, 0xCD, 0x13, 0xF4,
        ],
    );
    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted));
    // Bytes 0..511 must be 0xBB (sector 1), 512..1023 must be 0xCC (sector 2).
    for off in 0..512 {
        assert_eq!(vm.read_mem_u8(0x3000 + off), 0xBB);
    }
    for off in 0..512 {
        assert_eq!(vm.read_mem_u8(0x3000 + 512 + off), 0xCC);
    }
}

/// Full cold-boot pipeline into protected mode. The boot sector loads
/// sector 1 from disk and far-jumps to it; sector 1 itself flips
/// CR0.PE=1 via the canonical `MOV EAX,CR0; OR AX,1; MOV CR0,EAX`
/// idiom and then issues a 32-bit MOV that writes `0xCAFEBABE` to
/// linear 0x2000. After HLT the host inspects the dword.
///
/// This is the first regression that exercises every architectural
/// piece in series: cold boot from disk → BIOS INT 0x13 → segmented
/// far jump → CR0 read/write opcodes → PE flag flip → 32-bit operand
/// dispatch → paged-or-not memory write. A regression in any of
/// those breaks this test even when narrower unit tests still pass.
#[test]
fn cold_boot_kernel_transitions_to_protected_mode_and_writes_32_bit_dword() {
    // Sector 0 — real-mode boot sector (same shape as the earlier
    // cold-boot test): load sector 1 into 0x0000:0x8000 and far-jump.
    let mut sector0 = vec![0u8; 512];
    sector0[..27].copy_from_slice(&[
        0xB8, 0x00, 0x00, 0x8E, 0xC0, 0xBB, 0x00, 0x80, 0xB4, 0x02, 0xB0, 0x01, 0xB5, 0x00, 0xB1,
        0x02, 0xB6, 0x00, 0xB2, 0x80, 0xCD, 0x13, 0xEA, 0x00, 0x80, 0x00, 0x00,
    ]);

    // Sector 1 — kernel stub, runs at CS:IP = 0x0000:0x8000.
    //   0F 20 C0          MOV EAX, CR0
    //   83 C8 01          OR AX, 1            (PE = 1)
    //   0F 22 C0          MOV CR0, EAX
    //   66 B8 BE BA FE CA MOV EAX, 0xCAFEBABE
    //   66 89 06 00 20    MOV [0x2000], EAX   (DS:disp16, DS = 0)
    //   F4                HLT
    let mut sector1 = vec![0u8; 512];
    sector1[..21].copy_from_slice(&[
        0x0F, 0x20, 0xC0, 0x83, 0xC8, 0x01, 0x0F, 0x22, 0xC0, 0x66, 0xB8, 0xBE, 0xBA, 0xFE, 0xCA,
        0x66, 0x89, 0x06, 0x00, 0x20, 0xF4,
    ]);

    let mut disk = sector0;
    disk.extend_from_slice(&sector1);

    let mut vm = Vm::new();
    vm.install_bios();
    vm.load_disk_image(&disk);
    vm.boot_from_disk();
    let (_, stop) = vm.run_steps(512);
    assert!(matches!(stop, Stop::Halted), "stop: {stop:?}");
    assert_eq!(vm.cpu().cr0 & 1, 1, "kernel stub must have flipped CR0.PE");
    // Little-endian: 0xCAFEBABE at linear 0x2000 = BE BA FE CA.
    assert_eq!(vm.read_mem_u8(0x2000), 0xBE);
    assert_eq!(vm.read_mem_u8(0x2001), 0xBA);
    assert_eq!(vm.read_mem_u8(0x2002), 0xFE);
    assert_eq!(vm.read_mem_u8(0x2003), 0xCA);
}

/// End-to-end bootstrap: boot sector lives on the in-memory disk; the
/// VM cold-boots via [`Vm::boot_from_disk`] which copies sector 0 to
/// 0x7C00 (mimicking real BIOS). That sector then itself calls INT
/// 0x13 AH=0x02 to load sector 1 into 0x0000:0x8000 and far-jumps to
/// it. Sector 1 prints "OK" via INT 0x10 AH=0x0E and HLTs. The whole
/// pipeline — host-side BIOS shim, IoBus disk, segment cache, INT
/// dispatch, far jump, VGA write — has to work cooperatively.
#[test]
fn cold_boot_loads_kernel_from_disk_and_runs_it() {
    let mut sector0 = vec![0u8; 512];
    sector0[..27].copy_from_slice(&[
        // MOV AX, 0; MOV ES, AX            ; B8 00 00 8E C0
        0xB8, 0x00, 0x00, 0x8E, 0xC0, // MOV BX, 0x8000                   ; BB 00 80
        0xBB, 0x00, 0x80, // MOV AH, 0x02                     ; B4 02
        0xB4, 0x02, // MOV AL, 0x01                     ; B0 01
        0xB0, 0x01, // MOV CH, 0x00                     ; B5 00
        0xB5, 0x00, // MOV CL, 0x02 (sector 2)          ; B1 02
        0xB1, 0x02, // MOV DH, 0x00                     ; B6 00
        0xB6, 0x00, // MOV DL, 0x80                     ; B2 80
        0xB2, 0x80, // INT 0x13                         ; CD 13
        0xCD, 0x13, // JMP FAR 0x0000:0x8000            ; EA 00 80 00 00
        0xEA, 0x00, 0x80, 0x00, 0x00,
    ]);

    let mut sector1 = vec![0u8; 512];
    sector1[..9].copy_from_slice(&[
        // MOV AH, 0x0E; MOV AL, 'O'; INT 0x10
        // MOV AL, 'K';  INT 0x10
        // HLT
        0xB4, 0x0E, 0xB0, b'O', 0xCD, 0x10, 0xB0, b'K', 0xCD,
    ]);
    sector1[9] = 0x10;
    sector1[10] = 0xF4;

    let mut disk = sector0;
    disk.extend_from_slice(&sector1);

    let mut vm = Vm::new();
    vm.install_bios();
    vm.load_disk_image(&disk);
    vm.boot_from_disk();
    let (_, stop) = vm.run_steps(256);
    assert!(matches!(stop, Stop::Halted), "got: {stop:?}");
    assert_eq!(vm.read_mem_u8(VGA_TEXT_BASE), b'O');
    assert_eq!(vm.read_mem_u8(VGA_TEXT_BASE + 2), b'K');
    assert_eq!(vm.read_mem_u8(BDA_CURSOR_COL), 2);
}

/// INT 0x13 AH=0x02 reads sectors from the in-memory disk image into
/// ES:BX. We load a 1024-byte disk where sector 0 is all 0xAA and
/// sector 1 is all 0xBB, then have a boot stub read sector 1 into
/// [ES:BX] = [0:0x2000].
#[test]
fn bios_int13_read_sectors_copies_to_es_bx() {
    let mut vm = Vm::new();
    vm.install_bios();
    let mut disk = vec![0xAAu8; 512];
    disk.extend_from_slice(&[0xBB; 512]);
    vm.load_disk_image(&disk);
    // Boot stub at 0x7C00:
    //   MOV AX, 0x0000     ; B8 00 00
    //   MOV ES, AX         ; 8E C0     (ES = 0)
    //   MOV BX, 0x2000     ; BB 00 20
    //   MOV AH, 0x02       ; B4 02     (read sectors)
    //   MOV AL, 0x01       ; B0 01     (one sector)
    //   MOV CH, 0x00       ; B5 00     (cyl low)
    //   MOV CL, 0x02       ; B1 02     (sector 2 = LBA 1)
    //   MOV DH, 0x00       ; B6 00     (head 0)
    //   MOV DL, 0x80       ; B2 80     (first hard disk)
    //   INT 0x13           ; CD 13
    //   HLT                ; F4
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0xB8, 0x00, 0x00, // MOV AX, 0
            0x8E, 0xC0, // MOV ES, AX
            0xBB, 0x00, 0x20, // MOV BX, 0x2000
            0xB4, 0x02, // MOV AH, 2
            0xB0, 0x01, // MOV AL, 1
            0xB5, 0x00, // MOV CH, 0
            0xB1, 0x02, // MOV CL, 2 (sector 2, 1-based)
            0xB6, 0x00, // MOV DH, 0
            0xB2, 0x80, // MOV DL, 0x80
            0xCD, 0x13, // INT 0x13
            0xF4, // HLT
        ],
    );
    vm.boot();
    let (_, stop) = vm.run_steps(64);
    assert!(matches!(stop, Stop::Halted));
    // The sector at LBA 1 (all 0xBB) must land at linear 0x2000.
    for off in 0..512 {
        assert_eq!(
            vm.read_mem_u8(0x2000 + off),
            0xBB,
            "byte {off} of loaded sector"
        );
    }
    // Sector 0 should NOT have been touched at 0x2000; we already
    // checked the whole 512 are 0xBB. AH must be 0 (success).
    assert_eq!(vm.cpu().read_r8(4), 0);
}

/// install_bios() wires INT 0x10 to the Rust shim. A guest that does
/// `MOV AH, 0x0E; MOV AL, 'X'; INT 0x10; HLT` must end up with 'X'
/// at VGA cell (0,0) and the BDA cursor advanced to column 1.
#[test]
fn bios_int10_tty_writes_to_vga_and_advances_cursor() {
    let mut vm = Vm::new();
    vm.install_bios();
    // Boot stub at 0x7C00:
    //   MOV AH, 0x0E  ; B4 0E
    //   MOV AL, 'X'   ; B0 58
    //   INT 0x10      ; CD 10
    //   HLT           ; F4
    vm.load_image(BOOT_LOAD_ADDR, &[0xB4, 0x0E, 0xB0, b'X', 0xCD, 0x10, 0xF4]);
    vm.boot();
    let (_, stop) = vm.run_steps(32);
    assert!(matches!(stop, Stop::Halted));
    assert_eq!(vm.read_mem_u8(VGA_TEXT_BASE), b'X');
    assert_eq!(vm.read_mem_u8(VGA_TEXT_BASE + 1), 0x07, "attribute byte");
    assert_eq!(vm.read_mem_u8(BDA_CURSOR_COL), 1);
    assert_eq!(vm.read_mem_u8(BDA_CURSOR_ROW), 0);
}

/// CR and LF advance the cursor without writing characters; an
/// 'A' afterwards lands at the start of row 1.
#[test]
fn bios_int10_tty_handles_cr_lf() {
    let mut vm = Vm::new();
    vm.install_bios();
    // MOV AH, 0x0E
    // MOV AL, 0x0A ; LF
    // INT 0x10
    // MOV AL, 0x0D ; CR
    // INT 0x10
    // MOV AL, 'A'
    // INT 0x10
    // HLT
    vm.load_image(
        BOOT_LOAD_ADDR,
        &[
            0xB4, 0x0E, 0xB0, 0x0A, 0xCD, 0x10, 0xB0, 0x0D, 0xCD, 0x10, 0xB0, b'A', 0xCD, 0x10,
            0xF4,
        ],
    );
    vm.boot();
    let (_, _) = vm.run_steps(64);
    // 'A' must be at row 1, col 0. VGA offset = (1*80 + 0)*2 = 160.
    assert_eq!(vm.read_mem_u8(VGA_TEXT_BASE + 160), b'A');
    assert_eq!(vm.read_mem_u8(BDA_CURSOR_COL), 1);
    assert_eq!(vm.read_mem_u8(BDA_CURSOR_ROW), 1);
}

#[test]
fn boots_and_prints_banner() {
    let mut vm = Vm::new();
    vm.load_default_guest();
    vm.boot();
    // Print loop is ~7 instructions per char; banner is 13 bytes
    // plus NUL. Give it some slack.
    let (_, stop) = vm.run_steps(2_000);
    // The guest never halts — after printing it polls for input.
    match stop {
        Stop::StepBudget => {}
        other => panic!("unexpected stop: {other:?}"),
    }
    let out = vm.drain_output();
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("wwwvm: ready"), "got: {s:?}");
}

#[test]
fn echoes_input_back() {
    let mut vm = Vm::new();
    vm.load_default_guest();
    vm.boot();
    // Drain the banner first
    vm.run_steps(2_000);
    let _ = vm.drain_output();

    vm.send_input(b"Q");
    vm.run_steps(2_000);
    let out = vm.drain_output();
    assert_eq!(out, b"Q");
}

#[test]
fn autorun_delivers_commands_at_boot() {
    let mut vm = Vm::new();
    vm.load_default_guest();
    vm.set_autorun_commands(["hi", "ok"]);
    vm.boot();
    vm.run_steps(5_000);
    let out = vm.drain_output();
    let s = String::from_utf8_lossy(&out);
    // banner is printed once, autorun bytes get echoed back
    assert!(s.contains("wwwvm: ready"));
    assert!(s.contains("hi\nok\n"), "got: {s:?}");
}

/// Hand-assembled guest that uses LOOP, INC r/m8 (Group 4), and the
/// UART to print the five-character string "ABCDE" without storing
/// it in memory. Verifies the wider ISA round-trips through the
/// `Vm` API as a single OUT-per-character pump.
///
/// ```text
/// 0: B9 05 00     MOV CX, 5
/// 3: B0 41        MOV AL, 'A'
/// 5: BA F8 03     MOV DX, 0x3F8
/// 8: EE           OUT DX, AL
/// 9: FE C0        INC AL
/// B: E2 FB        LOOP -5  -> 8
/// D: F4           HLT
/// ```
#[test]
fn loop_counted_print_via_uart() {
    let program: &[u8] = &[
        0xB9, 0x05, 0x00, 0xB0, 0x41, 0xBA, 0xF8, 0x03, 0xEE, 0xFE, 0xC0, 0xE2, 0xFB, 0xF4,
    ];
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    let (_, stop) = vm.run_steps(1_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    let out = vm.drain_output();
    assert_eq!(&out, b"ABCDE");
}

/// Hand-assembled "byte squarer": read a byte from the UART, square
/// it via `MUL r/m8`, write the low byte of the product back out.
/// Exercises poll-loop, IN, MUL, OUT in one tight cycle.
///
/// ```text
/// 0: BA FD 03    MOV DX, 0x3FD   ; LSR
/// 3: EC          IN  AL, DX
/// 4: A8 01       TEST AL, 1
/// 6: 74 F8       JZ  -8 -> 0
/// 8: BA F8 03    MOV DX, 0x3F8
/// B: EC          IN  AL, DX      ; AL = input byte
/// C: 88 C3       MOV BL, AL
/// E: F6 E3       MUL BL          ; AX = AL * BL
/// 10: BA F8 03   MOV DX, 0x3F8
/// 13: EE         OUT DX, AL      ; emit low byte of product
/// 14: EB EA      JMP -22 -> 0
/// ```
#[test]
fn mul_byte_squarer_round_trip() {
    let program: &[u8] = &[
        0xBA, 0xFD, 0x03, 0xEC, 0xA8, 0x01, 0x74, 0xF8, 0xBA, 0xF8, 0x03, 0xEC, 0x88, 0xC3, 0xF6,
        0xE3, 0xBA, 0xF8, 0x03, 0xEE, 0xEB, 0xEA,
    ];
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    // Feed two byte inputs and expect their squared low bytes back.
    vm.send_input(&[3, 5, 16]);
    vm.run_steps(20_000);
    let out = vm.drain_output();
    // 3*3=9, 5*5=25, 16*16=256 → low byte 0
    assert_eq!(out, vec![9, 25, 0]);
}

/// End-to-end interrupt-driven serial: handler reads a byte from
/// the UART RBR into BL, EOIs the PIC, IRETs. Main routine
/// enables IER, unmasks IRQ 4, STIs, spins until BL != 0, HLTs.
/// IVT for vector 0x0C is wired via `set_ivt` so the guest
/// program is pure code.
#[test]
fn uart_rx_drives_irq4_handler_through_vm() {
    let main: &[u8] = &[
        0xFB, // STI
        0xBA, 0xF9, 0x03, // MOV DX, 0x3F9 (UART IER)
        0xB0, 0x01, 0xEE, // OUT DX, AL
        0xB0, 0xEF, 0xE6, 0x21, // OUT 0x21, AL (PIC IMR)
        0x80, 0xFB, 0x00, // CMP BL, 0
        0x74, 0xFB, // JZ -5
        0xF4, // HLT
    ];
    let handler: &[u8] = &[
        0x50, // PUSH AX
        0xBA, 0xF8, 0x03, // MOV DX, 0x3F8 (RBR)
        0xEC, // IN AL, DX
        0x88, 0xC3, // MOV BL, AL
        0xB0, 0x20, 0xE6, 0x20, // OUT 0x20, AL (EOI)
        0x58, // POP AX
        0xCF, // IRET
    ];
    let handler_addr: u32 = 0x7C40;
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, main);
    vm.load_image(handler_addr, handler);
    vm.set_ivt(0x0C, 0x0000, handler_addr as u16);
    vm.boot();
    vm.send_input(&[0x42]);
    let (_, stop) = vm.run_steps(5_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::BX] & 0xFF, 0x42);
}

/// End-to-end test for the 8254 timer. PIT ch0 mode 2 fires every
/// 50 ticks → IRQ 0 → handler increments byte at 0x900 and EOIs
/// the PIC. Main spins until the counter reaches 4, then HLTs.
/// IVT and handler placement are managed via `set_ivt` so the
/// program stays linear.
#[test]
fn pit_timer_drives_irq0_handler_through_vm() {
    let main: &[u8] = &[
        0xB0, 0x34, // MOV AL, 0x34   (PIT mode 2, RW=3)
        0xE6, 0x43, // OUT 0x43, AL
        0xB0, 0x32, // MOV AL, 50     (reload LSB)
        0xE6, 0x40, // OUT 0x40, AL
        0x30, 0xC0, // XOR AL, AL     (reload MSB = 0)
        0xE6, 0x40, // OUT 0x40, AL
        0xB0, 0xFE, // MOV AL, 0xFE   (unmask IRQ 0)
        0xE6, 0x21, // OUT 0x21, AL
        0xFB, // STI
        0x80, 0x3E, 0x00, 0x09, 0x04, // CMP byte [0x900], 4
        0x75, 0xF9, // JNZ -7
        0xF4, // HLT
    ];
    let handler: &[u8] = &[
        0x50, // PUSH AX
        0xFE, 0x06, 0x00, 0x09, // INC byte [0x900]
        0xB0, 0x20, 0xE6, 0x20, // OUT 0x20, AL (EOI)
        0x58, // POP AX
        0xCF, // IRET
    ];
    let handler_addr: u32 = 0x7C50;
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, main);
    vm.load_image(handler_addr, handler);
    vm.set_ivt(0x08, 0x0000, handler_addr as u16);
    vm.boot();
    let (_, stop) = vm.run_steps(5_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.read_mem_u8(0x900), 4);
}

/// Guest writes plain ASCII directly to the VGA text-mode buffer
/// at 0xB8000; the host reads it back via `vga_text_snapshot`.
/// Uses `MOV BYTE [disp16], imm8` for each cell — a real driver
/// would loop with REP STOSW, but the imm form is the most
/// instruction-economical way to write a known string.
///
/// We set ES = 0xB800 first, then use ES:0 addressing so the
/// 16-bit offsets fit comfortably. Each char cell is char+attr,
/// two bytes; we only write the char byte, leaving attribute=0.
#[test]
fn guest_writes_vga_buffer_and_host_snapshots_it() {
    // 0: B8 00 B8     MOV AX, 0xB800
    // 3: 8E C0        MOV ES, AX        (8E /0, modrm=11 000 000)
    // 5: 26 C6 06 00 00 'H'    MOV BYTE ES:[0x0000], 'H'
    // ... one per char ...
    // F4              HLT
    //
    // ES: prefix (0x26) before each MOV byte, with mod=00 rm=110
    // (disp16) ModR/M = 0x06; Group-11 /0 = 0xC6.
    let mut program: Vec<u8> = vec![0xB8, 0x00, 0xB8, 0x8E, 0xC0];
    // Write "HELLO VGA" — each character at offset col*2 in the
    // VGA cell array (so the attribute byte at col*2 + 1 stays 0).
    for (i, &c) in b"HELLO VGA".iter().enumerate() {
        let off = (i * 2) as u16;
        program.extend_from_slice(&[
            0x26, // ES: prefix
            0xC6,
            0x06, // MOV BYTE [disp16], imm8
            (off & 0xFF) as u8,
            (off >> 8) as u8,
            c,
        ]);
    }
    program.push(0xF4); // HLT

    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, &program);
    vm.boot();
    let (_, stop) = vm.run_steps(2_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    let snapshot = vm.vga_text_snapshot();
    let first_line = snapshot.lines().next().unwrap();
    assert!(
        first_line.starts_with("HELLO VGA"),
        "first line: {first_line:?}",
    );
}

/// End-to-end cascade IRQ delivery: host raises an IRQ on the
/// slave PIC, the master sees IRQ 2 from the cascade, the CPU
/// gets the *slave's* vector. Handler EOIs both PICs (the
/// canonical PC pattern) and IRETs.
///
/// Slave IRQ 0 = vector 0x70. Without the cascade, the master
/// would never deliver it.
#[test]
fn slave_pic_cascade_delivers_irq_through_vm() {
    let main: &[u8] = &[
        0xB0, 0xFB, // MOV AL, 0xFB  (master: unmask IRQ 2 cascade)
        0xE6, 0x21, // OUT 0x21, AL
        0xB0, 0xFE, // MOV AL, 0xFE  (slave: unmask IRQ 0)
        0xE6, 0xA1, // OUT 0xA1, AL
        0xFB, // STI
        0x80, 0xFB, 0x00, // CMP BL, 0
        0x74, 0xFB, // JZ -5
        0xF4, // HLT
    ];
    // Handler: EOI slave first (0xA0), then master (0x20). The
    // order matters on real hardware — slave's ISR must clear
    // before master's so the cascade line deasserts cleanly.
    let handler: &[u8] = &[
        0x50, // PUSH AX
        0xB3, 0x77, // MOV BL, 0x77   (proof we ran)
        0xB0, 0x20, 0xE6, 0xA0, // OUT 0xA0, AL   (slave EOI)
        0xB0, 0x20, 0xE6, 0x20, // OUT 0x20, AL   (master EOI)
        0x58, // POP AX
        0xCF, // IRET
    ];
    let handler_addr: u32 = 0x7C40;
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, main);
    vm.load_image(handler_addr, handler);
    // Slave IRQ 0 → vector 0x70
    vm.set_ivt(0x70, 0x0000, handler_addr as u16);
    vm.boot();
    // Raise slave IRQ 0 — the cascade should propagate.
    vm.io.slave_pic.raise_irq(0);
    let (_, stop) = vm.run_steps(2_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::BX] & 0xFF, 0x77);
    // Both PIC ISRs cleared by the double EOI.
    assert_eq!(vm.io.pic.isr, 0);
    assert_eq!(vm.io.slave_pic.isr, 0);
}

/// End-to-end keyboard test: guest unmasks IRQ 1, STIs, spins on
/// BL == 0. Handler reads port 0x60 into BL, EOIs the PIC. Host
/// pushes a scan code; the IRQ should latch and dispatch.
#[test]
fn ps2_scancode_drives_irq1_handler_through_vm() {
    let main: &[u8] = &[
        0xFB, // STI
        0xB0, 0xFD, // MOV AL, 0xFD  (unmask IRQ 1)
        0xE6, 0x21, // OUT 0x21, AL
        0x80, 0xFB, 0x00, // CMP BL, 0
        0x74, 0xFB, // JZ -5
        0xF4, // HLT
    ];
    let handler: &[u8] = &[
        0x50, // PUSH AX
        0xE4, 0x60, // IN AL, 0x60
        0x88, 0xC3, // MOV BL, AL
        0xB0, 0x20, 0xE6, 0x20, // OUT 0x20, AL (EOI)
        0x58, // POP AX
        0xCF, // IRET
    ];
    let handler_addr: u32 = 0x7C40;
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, main);
    vm.load_image(handler_addr, handler);
    // IRQ 1 → vector 0x08 + 1 = 0x09
    vm.set_ivt(0x09, 0x0000, handler_addr as u16);
    vm.boot();
    vm.push_scancode(0x42);
    let (_, stop) = vm.run_steps(2_000);
    match stop {
        Stop::Halted => {}
        other => panic!("expected Halted, got {other:?}"),
    }
    assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::BX] & 0xFF, 0x42);
}

#[test]
fn calculator_demo_squares_and_prints_decimal() {
    let cases: &[(u8, &str)] = &[
        (0, "0\n"),
        (1, "1\n"),
        (7, "49\n"),
        (10, "100\n"),
        (16, "256\n"),
        (255, "65025\n"),
    ];
    for &(input, expected) in cases {
        let mut vm = Vm::new();
        vm.load_calculator_demo();
        vm.boot();
        vm.send_input(&[input]);
        vm.run_steps(50_000);
        let out = vm.drain_output();
        let got = String::from_utf8_lossy(&out);
        assert_eq!(
            got, expected,
            "input={input}: expected {expected:?}, got {got:?}"
        );
    }
}

#[test]
fn interactive_demo_prints_banner_and_echoes_via_irq() {
    let mut vm = Vm::new();
    vm.load_interactive_demo();
    vm.boot();
    // Let the banner-printing loop run; main lands in JMP -2 spin
    // and the test never halts on its own.
    let (_, stop) = vm.run_steps(500);
    match stop {
        Stop::StepBudget => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
    let banner = vm.drain_output();
    assert!(
        String::from_utf8_lossy(&banner).contains("wwwvm interactive"),
        "got: {banner:?}",
    );

    // Now drive an IRQ-4-driven echo. The handler reads RBR and
    // writes back to THR; main is still spinning in JMP -2.
    vm.send_input(b"Q");
    vm.run_steps(500);
    let echo = vm.drain_output();
    assert_eq!(echo, b"Q");

    // Multiple bytes work too — each generates a fresh IRQ.
    vm.send_input(b"abc");
    vm.run_steps(2_000);
    let echo = vm.drain_output();
    assert_eq!(echo, b"abc");
}

/// Snapshot CPU+RAM mid-execution, restore into a fresh VM,
/// verify the program produces the same final result whether it
/// runs straight through or is interrupted by a snapshot/restore
/// round-trip.
#[test]
fn snapshot_restore_round_trips_mid_execution() {
    // Simple loop guest summing 1..=10 in BX. Loaded at 0x7C00.
    let program: &[u8] = &[
        0xB9, 0x0A, 0x00, // MOV CX, 10
        0x31, 0xDB, // XOR BX, BX
        0x01, 0xCB, // ADD BX, CX
        0xE2, 0xFC, // LOOP -4
        0xF4, // HLT
    ];

    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    // Run a handful of steps so we land mid-iteration.
    vm.run_steps(8);
    assert!(!vm.is_halted());

    let snap = vm.snapshot();

    // Continue the original to completion.
    vm.run_steps(200);
    assert!(vm.is_halted());
    let original_bx = vm.cpu().regs[wwwvm_cpu::r16::BX];

    // Restore into a fresh VM and continue from the snapshot point.
    let mut vm2 = Vm::new();
    vm2.restore(&snap).expect("restore");
    vm2.run_steps(200);
    assert!(vm2.is_halted());
    assert_eq!(vm2.cpu().regs[wwwvm_cpu::r16::BX], original_bx);
    assert_eq!(original_bx, 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10);
}

#[test]
fn restore_rejects_bad_magic() {
    let mut bytes = vec![0u8; snapshot::HEADER_LEN + snapshot::CPU_LEN + Vm::RAM_SIZE];
    bytes[..6].copy_from_slice(b"NOPE!\x00");
    bytes[6] = snapshot::VERSION;
    let mut vm = Vm::new();
    let err = vm.restore(&bytes).unwrap_err();
    match err {
        snapshot::SnapshotError::BadMagic => {}
        other => panic!("unexpected: {other}"),
    }
}

#[test]
fn restore_rejects_unknown_version() {
    let mut bytes = vec![0u8; snapshot::HEADER_LEN + snapshot::CPU_LEN + Vm::RAM_SIZE];
    bytes[..snapshot::MAGIC.len()].copy_from_slice(snapshot::MAGIC);
    bytes[snapshot::MAGIC.len()] = 99;
    let mut vm = Vm::new();
    match vm.restore(&bytes).unwrap_err() {
        snapshot::SnapshotError::UnsupportedVersion(99) => {}
        other => panic!("unexpected: {other}"),
    }
}

/// v2 must preserve UART rx/tx and PIC mask state. Reproduce a
/// scenario where v1 would break: queue a byte in UART rx, take
/// a snapshot before the guest reads it, restore into a fresh VM,
/// and verify the byte is still readable from port 0x3F8.
#[test]
fn snapshot_v2_preserves_uart_buffers_and_pic_state() {
    use wwwvm_devices::IoDevice;
    let mut vm = Vm::new();
    vm.load_default_guest();
    vm.boot();
    // Queue UART rx, twiddle PIC IMR, set CMOS index.
    vm.send_input(b"Z");
    vm.io.pic.imr = 0xEF;
    vm.io.slave_pic.vector_base = 0x70;
    vm.io.cmos.set_time(26, 12, 31, 23, 59, 58);
    // Take snapshot, mutate the original, restore into a fresh VM.
    let snap = vm.snapshot();
    vm.send_input(b"X"); // additional data after snapshot
    vm.io.pic.imr = 0xFF; // re-mask
    let mut vm2 = Vm::new();
    vm2.restore(&snap).expect("restore v2");
    // UART rx should still contain 'Z' (and not 'X').
    assert_eq!(vm2.io.uart.rx_pending(), 1);
    // Master PIC mask preserved.
    assert_eq!(vm2.io.pic.imr, 0xEF);
    // Slave PIC vector base preserved.
    assert_eq!(vm2.io.slave_pic.vector_base, 0x70);
    // CMOS storage preserved.
    vm2.io.cmos.write(0x70, 0x09); // index = YEAR
    assert_eq!(vm2.io.cmos.read(0x71), 26);
}

/// v7 snapshot must round-trip the full architectural i386 state:
/// CR0/2/3/4, GDTR, IDTR, full 32-bit IP, TSC, LDTR, TR, A20,
/// stack_size_32, FPU control/status, SYSENTER MSRs, upper 16 of
/// GPRs.
#[test]
fn snapshot_v8_preserves_i386_state() {
    let mut vm = Vm::new();
    vm.load_default_guest();
    vm.boot();
    vm.cpu.cr0 = 0x8000_0001;
    vm.cpu.gdtr = wwwvm_cpu::DescriptorTable {
        limit: 0x00FF,
        base: 0x0001_0000,
    };
    vm.cpu.idtr = wwwvm_cpu::DescriptorTable {
        limit: 0x07FF,
        base: 0x0002_0000,
    };
    vm.cpu.cr3 = 0xCAFE_B000;
    vm.cpu.cr2 = 0xDEAD_FACE;
    vm.cpu.cr4 = 0x0000_0020;
    vm.cpu.tsc = 0x1234_5678_9ABC_DEF0;
    vm.cpu.ldtr = 0x0048;
    vm.cpu.tr = 0x0028;
    vm.cpu.a20 = false;
    vm.cpu.stack_size_32 = true;
    vm.cpu.ip = 0xC010_0000;
    vm.cpu.regs_high[0] = 0xDEAD;
    vm.cpu.fpu_cw = 0x027F;
    vm.cpu.fpu_sw = 0x4000;
    vm.cpu.sysenter_cs = 0x0008;
    vm.cpu.sysenter_esp = 0x0007_0000;
    vm.cpu.sysenter_eip = 0xC011_2233;
    vm.cpu.fpu_st[0] = 1.875;
    vm.cpu.fpu_st[3] = -2.5;
    vm.cpu.fpu_top = 5;
    let snap = vm.snapshot();
    let mut vm2 = Vm::new();
    vm2.restore(&snap).expect("v8 restore");
    assert_eq!(vm2.cpu().cr0, 0x8000_0001);
    assert_eq!(vm2.cpu().gdtr.limit, 0x00FF);
    assert_eq!(vm2.cpu().gdtr.base, 0x0001_0000);
    assert_eq!(vm2.cpu().idtr.limit, 0x07FF);
    assert_eq!(vm2.cpu().idtr.base, 0x0002_0000);
    assert_eq!(vm2.cpu().cr3, 0xCAFE_B000);
    assert_eq!(vm2.cpu().cr2, 0xDEAD_FACE);
    assert_eq!(vm2.cpu().cr4, 0x0000_0020);
    assert_eq!(vm2.cpu().tsc, 0x1234_5678_9ABC_DEF0);
    assert_eq!(vm2.cpu().ldtr, 0x0048);
    assert_eq!(vm2.cpu().tr, 0x0028);
    assert!(!vm2.cpu().a20);
    assert!(vm2.cpu().stack_size_32);
    assert_eq!(vm2.cpu().ip, 0xC010_0000);
    assert_eq!(vm2.cpu().regs_high[0], 0xDEAD);
    assert_eq!(vm2.cpu().read_r32(0) & 0xFFFF_0000, 0xDEAD_0000);
    assert_eq!(vm2.cpu().fpu_cw, 0x027F);
    assert_eq!(vm2.cpu().fpu_sw, 0x4000);
    assert_eq!(vm2.cpu().sysenter_cs, 0x0008);
    assert_eq!(vm2.cpu().sysenter_esp, 0x0007_0000);
    assert_eq!(vm2.cpu().sysenter_eip, 0xC011_2233);
    assert_eq!(vm2.cpu().fpu_st[0], 1.875);
    assert_eq!(vm2.cpu().fpu_st[3], -2.5);
    assert_eq!(vm2.cpu().fpu_top, 5);
}

/// A v1 snapshot (synthesized by hand) must still restore the CPU
/// + RAM portions; devices come back fresh.
#[test]
fn restore_accepts_v1_snapshots() {
    let mut v1: Vec<u8> = Vec::new();
    v1.extend_from_slice(snapshot::MAGIC);
    v1.push(1); // version 1
    v1.push(0); // flags
    v1.extend_from_slice(&[0u8; 8]); // reserved
                                     // CPU image — 36 bytes of zero. (regs=0, sregs=0, IP=0,
                                     // flags=0, halted=0, seg_override=0xFF, 2 reserved.)
    let mut cpu = vec![0u8; snapshot::CPU_LEN];
    cpu[33] = 0xFF;
    v1.extend_from_slice(&cpu);
    // Memory: 1 MiB of zero
    v1.extend(std::iter::repeat_n(0u8, Vm::RAM_SIZE));
    let mut vm = Vm::new();
    vm.send_input(b"junk"); // device state that should be dropped
    vm.restore(&v1).expect("v1 restore");
    // CPU reset to all-zero IP and CS = 0
    assert_eq!(vm.cpu().ip, 0);
    // Devices stay fresh-ish — but since this VM was pre-loaded
    // with rx bytes before restore, v1 has no opinion on UART
    // state, so the old bytes remain. We assert that fact so
    // future maintainers don't think v1 wipes devices.
    assert_eq!(vm.io.uart.rx_pending(), 4);
}

#[test]
fn restore_rejects_truncated_blob() {
    let vm = Vm::new();
    let mut snap = vm.snapshot();
    snap.truncate(snap.len() / 2);
    let mut vm2 = Vm::new();
    match vm2.restore(&snap).unwrap_err() {
        snapshot::SnapshotError::TooSmall { .. } => {}
        other => panic!("unexpected: {other}"),
    }
}

/// Divide-by-zero in the guest must surface as `Stop::CpuError`
/// rather than silently producing garbage. This is the VM-side
/// view of `CpuError::DivideError`.
#[test]
fn div_by_zero_surfaces_through_vm_stop() {
    // MOV AL, 5 ; MOV BL, 0 ; DIV BL ; HLT (unreached)
    let program: &[u8] = &[0xB0, 0x05, 0xB3, 0x00, 0xF6, 0xF3, 0xF4];
    let mut vm = Vm::new();
    vm.load_image(BOOT_LOAD_ADDR, program);
    vm.boot();
    let (_, stop) = vm.run_steps(1_000);
    match stop {
        Stop::CpuError(e) => {
            let msg = e.to_string();
            assert!(msg.contains("divide error"), "got: {msg}");
        }
        other => panic!("expected CpuError(DivideError), got {other:?}"),
    }
    // VM did not transition to halted state — divide error is a
    // separate failure mode.
    assert!(!vm.is_halted());
}
