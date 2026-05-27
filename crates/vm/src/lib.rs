//! VM orchestrator: owns CPU + memory + IO bus, drives the fetch loop,
//! and exposes a small high-level API used by the wasm bindings.
//!
//! The crate also ships a tiny hand-assembled real-mode guest payload
//! (`HELLO_GUEST`) — it prints a banner over the UART and echoes any
//! input back. That payload is the proof-of-pipeline used by the demo
//! while the CPU/devices grow towards running real OS images.

#![forbid(unsafe_code)]

pub mod bzimage;
pub mod elf;

pub use bzimage::{parse as parse_bzimage, BzImage, BzImageError};
pub use elf::{load_elf, ElfError};
use wwwvm_cpu::{Cpu, CpuError};
use wwwvm_devices::IoBus;
use wwwvm_mem::Memory;

/// Standard boot-sector load address on x86.
pub const BOOT_LOAD_ADDR: u32 = 0x7C00;

/// CGA/VGA text-mode buffer base (linear). 80 columns × 25 rows of
/// 2-byte cells (character + attribute) lives here. Guests write
/// directly via MOV instructions; the host reads it back with
/// [`Vm::vga_text_snapshot`].
pub const VGA_TEXT_BASE: u32 = 0xB8000;
pub const VGA_TEXT_COLS: usize = 80;
pub const VGA_TEXT_ROWS: usize = 25;

/// BIOS Data Area cursor location for active page 0:
/// linear `0x450` = column, `0x451` = row. Real BIOS keeps all 8
/// pages in `0x450..0x460` (2 bytes each); we just touch page 0
/// because the host-side BIOS shim never changes the active page.
pub const BDA_CURSOR_COL: u32 = 0x0450;
pub const BDA_CURSOR_ROW: u32 = 0x0451;

/// Host-side BIOS shim. Installed via [`Vm::install_bios`]. When the
/// guest issues `INT n`, the CPU calls this with the vector; we
/// handle the small subset of BIOS calls the bundled guests use and
/// return `true` to short-circuit the normal IVT dispatch. Anything
/// we don't claim returns `false` and falls through to whatever the
/// guest installed at the IVT entry.
///
/// Currently implemented:
///   * INT 0x10 AH=0x0E — TTY teletype output. Writes AL to the VGA
///     text buffer at the BDA cursor (page 0), advances the cursor,
///     wraps at column 80 and clamps at row 24. CR/LF/BS are honored;
///     BEL is silently dropped.
pub fn bios_hook(cpu: &mut Cpu, mem: &mut Memory, io: &mut IoBus, vector: u8) -> bool {
    match vector {
        0x10 => bios_int10(cpu, mem),
        0x13 => bios_int13(cpu, mem, io),
        0x16 => bios_int16(cpu, io),
        _ => false,
    }
}

fn bios_int10(cpu: &mut Cpu, mem: &mut Memory) -> bool {
    let ah = cpu.read_r8(4); // AH lives in the high half of AX
    match ah {
        0x0E => {
            let al = cpu.read_r8(0);
            let mut col = mem.read_u8(BDA_CURSOR_COL) as usize;
            let mut row = mem.read_u8(BDA_CURSOR_ROW) as usize;
            match al {
                0x07 => { /* BEL — silently drop */ }
                0x08 => col = col.saturating_sub(1),
                b'\r' => col = 0,
                b'\n' => {
                    if row + 1 < VGA_TEXT_ROWS {
                        row += 1;
                    }
                }
                _ => {
                    let off = ((row * VGA_TEXT_COLS) + col) * 2;
                    mem.write_u8(VGA_TEXT_BASE + off as u32, al);
                    // Attribute byte: 0x07 = light-grey on black.
                    mem.write_u8(VGA_TEXT_BASE + off as u32 + 1, 0x07);
                    col += 1;
                    if col >= VGA_TEXT_COLS {
                        col = 0;
                        if row + 1 < VGA_TEXT_ROWS {
                            row += 1;
                        }
                    }
                }
            }
            mem.write_u8(BDA_CURSOR_COL, col as u8);
            mem.write_u8(BDA_CURSOR_ROW, row as u8);
            true
        }
        _ => false,
    }
}

/// INT 0x13 — disk services. We model:
///
///   * AH=0x02 — Read sectors. Inputs: AL = sector count, CH = cyl
///     bits 0..7, CL bits 6..7 = cyl bits 8..9, CL bits 0..5 = sector
///     (1-based!), DH = head, DL = drive (0x80 = boot drive). The
///     destination buffer is ES:BX.
///
/// The geometry is the canonical 1.44 MB floppy: 80 cylinders, 2
/// heads, 18 sectors/track. We use it for hard disks too, which is
/// wrong in general but fine for boot stubs that read by LBA via
/// `MOV CH,0; MOV CL,2` (cyl 0, sector 2) etc.
///
/// On return: CF=0 + AH=0 on success; CF=1 + AH=error on failure.
/// The flags update goes through `set_flag(flag::CF, …)` so it
/// survives any later state inspection.
fn bios_int13(cpu: &mut Cpu, mem: &mut Memory, io: &mut IoBus) -> bool {
    let ah = cpu.read_r8(4);
    match ah {
        0x02 => {
            let count = cpu.read_r8(0) as usize; // AL
            let ch = cpu.read_r8(5) as u32; // CH
            let cl = cpu.read_r8(1) as u32; // CL
            let dh = cpu.read_r8(6) as u32; // DH
            let _dl = cpu.read_r8(2); // DL (drive index — we have one disk)
            let cyl = ((cl & 0xC0) << 2) | ch;
            let sector = (cl & 0x3F).saturating_sub(1); // 1-based -> 0-based
            let head = dh;
            // CHS -> LBA: ((cyl * heads) + head) * sectors_per_track + sector.
            // Standard 1.44 MB floppy geometry.
            let lba = (cyl * 2 + head) * 18 + sector;
            // Destination linear = ES.base + BX. read_r16(3) returns BX.
            let bx = cpu.read_r16(3);
            let dest_linear = cpu.linear_seg(wwwvm_cpu::sreg::ES, bx);

            let mut buf = vec![0u8; count * wwwvm_devices::DISK_SECTOR_SIZE];
            io.disk.read_sectors(lba, count as u8, &mut buf);
            for (i, &b) in buf.iter().enumerate() {
                cpu.mem_write_u8(mem, dest_linear.wrapping_add(i as u32), b);
            }
            // Success: CF=0, AH=0. AL keeps the requested sector count.
            cpu.flags &= !wwwvm_cpu::flag::CF;
            cpu.write_r8(4, 0);
            true
        }
        _ => false,
    }
}

/// INT 0x16 — keyboard services. We model:
///
///   * AH=0x00 — Read keystroke. Blocking on real BIOS; here we
///     simulate the block by rewinding IP by 2 (back over the
///     `CD 16` we just decoded) when the keyboard queue is empty,
///     so the same INT re-executes next step. Once a byte is
///     available, we pop it and return AH=AL=byte (we don't yet
///     translate scan codes to ASCII).
///   * AH=0x01 — Check for keystroke. Non-blocking: ZF=1 if queue
///     empty, ZF=0 + AH/AL set (without popping) if a key waits.
fn bios_int16(cpu: &mut Cpu, io: &mut IoBus) -> bool {
    let ah = cpu.read_r8(4);
    match ah {
        0x00 => {
            if io.kbd.rx_pending() == 0 {
                // No key yet — rewind past `CD 16` so the next step
                // retries. The IP is currently after the imm8.
                cpu.ip = cpu.ip.wrapping_sub(2);
            } else {
                let byte = io.kbd.pop_scancode().unwrap_or(0);
                cpu.write_r8(0, byte); // AL
                cpu.write_r8(4, byte); // AH (scan code; we don't differentiate)
            }
            true
        }
        0x01 => {
            if let Some(byte) = io.kbd.peek_scancode() {
                cpu.flags &= !wwwvm_cpu::flag::ZF;
                cpu.write_r8(0, byte);
                cpu.write_r8(4, byte);
            } else {
                cpu.flags |= wwwvm_cpu::flag::ZF;
            }
            true
        }
        _ => false,
    }
}

/// Snapshot format constants and the error type used by `restore`.
pub mod snapshot {
    /// 6-byte format magic. Suitable for identifying the file from a
    /// hex dump.
    pub const MAGIC: &[u8] = b"WWWVM\x00";
    /// Current snapshot format version.
    /// * v1 — CPU (16-bit regs) + RAM only.
    /// * v2 — adds device state (UART/PIC/PIT/KBD/CMOS) after RAM.
    /// * v3 — adds i386 fields to the CPU image: upper 16 bits of
    ///   each GPR (regs_high), CR0, GDTR, IDTR.
    /// * v4 — appends CR3 (4 bytes) past the v3 layout — needed once
    ///   paging is in use, since CR3 names the page directory.
    /// * v5 — appends CR2 (4 bytes) past v4. CR2 holds the linear
    ///   address of the last page fault; preserving it across save/
    ///   restore lets a #PF handler see the address it was about to
    ///   service.
    pub const VERSION: u8 = 5;
    /// Bytes consumed by header: magic + version + flags + reserved.
    pub const HEADER_LEN: usize = 16;
    /// Bytes consumed by the v1/v2 CPU image — 8 r16 + 6 sreg + ip +
    /// flags + halted + seg_override + 2 reserved.
    pub const CPU_LEN: usize = 36;
    /// Extra bytes the v3 CPU image carries past the v1/v2 layout:
    /// 16 (regs_high u16 × 8) + 4 (cr0 u32) + 6 (gdtr) + 6 (idtr) = 32.
    pub const CPU_V3_EXTRA: usize = 32;
    /// Total bytes a v3 CPU image takes.
    pub const CPU_V3_LEN: usize = CPU_LEN + CPU_V3_EXTRA;
    /// Extra bytes v4 adds past the v3 image: 4 (cr3 u32).
    pub const CPU_V4_EXTRA: usize = 4;
    /// Total bytes a v4 CPU image takes.
    pub const CPU_V4_LEN: usize = CPU_V3_LEN + CPU_V4_EXTRA;
    /// Extra bytes v5 adds past v4: 4 (cr2 u32).
    pub const CPU_V5_EXTRA: usize = 4;
    /// Total bytes a v5 CPU image takes.
    pub const CPU_V5_LEN: usize = CPU_V4_LEN + CPU_V5_EXTRA;

    #[derive(Debug)]
    pub enum SnapshotError {
        TooSmall { got: usize, need: usize },
        BadMagic,
        UnsupportedVersion(u8),
        MemorySizeMismatch { expected: usize, actual: usize },
        DeviceRestore(String),
    }

    impl std::fmt::Display for SnapshotError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::TooSmall { got, need } => {
                    write!(
                        f,
                        "snapshot too small: got {got} bytes, need at least {need}"
                    )
                }
                Self::BadMagic => write!(f, "snapshot magic mismatch"),
                Self::UnsupportedVersion(v) => {
                    write!(f, "unsupported snapshot version {v}")
                }
                Self::MemorySizeMismatch { expected, actual } => {
                    write!(f, "memory size mismatch: expected {expected}, got {actual}")
                }
                Self::DeviceRestore(msg) => write!(f, "device restore failed: {msg}"),
            }
        }
    }

    impl std::error::Error for SnapshotError {}
}

/// Why the run loop stopped this turn.
#[derive(Debug)]
pub enum Stop {
    /// Hit a HLT and the CPU is parked. Further `run` calls are no-ops.
    Halted,
    /// Reached the cycle budget for this turn — call again to keep going.
    StepBudget,
    /// CPU could not decode the next instruction. Detail in the message.
    CpuError(CpuError),
}

pub struct Vm {
    cpu: Cpu,
    mem: Memory,
    io: IoBus,
    autorun: Vec<u8>,
    booted: bool,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// Default 1 MiB of conventional + low memory. Real-mode guests
    /// and the bundled demos fit here. For loading ELF kernels with
    /// entries above 0x100000, construct via [`Vm::with_ram_size`].
    pub const RAM_SIZE: usize = 0x10_0000;

    pub fn new() -> Self {
        Self::with_ram_size(Self::RAM_SIZE)
    }

    /// Construct a VM with a non-default RAM size. Use this when
    /// loading kernel images whose entry point lives above 1 MiB
    /// (typical Linux/BSD/Hobby-OS ELF kernels are linked at
    /// 0x00100000 or higher). Snapshot/restore still works as long
    /// as both ends use the *same* RAM size.
    pub fn with_ram_size(size: usize) -> Self {
        Self {
            cpu: Cpu::new(),
            mem: Memory::new(size),
            io: IoBus::new(),
            autorun: Vec::new(),
            booted: false,
        }
    }

    /// Copy bytes into physical RAM at `addr`. The same primitive
    /// powers `load_image` (a guest program), seeding data tables,
    /// and writing IVT entries.
    pub fn load_image(&mut self, addr: u32, bytes: &[u8]) {
        self.mem.write_slice(addr, bytes);
    }

    /// Write an IVT entry for the given vector. Vector `v` lives at
    /// linear address `v*4` as a 4-byte (offset, segment) record. JS
    /// callers use this to wire up handlers without emitting a string
    /// of `MOV WORD` instructions in the guest.
    pub fn set_ivt(&mut self, vector: u8, segment: u16, offset: u16) {
        let base = (vector as u32) * 4;
        self.mem.write_u16(base, offset);
        self.mem.write_u16(base + 2, segment);
    }

    /// Read a single byte from guest RAM. Useful for assertions in
    /// integration tests and for JS-side inspection of guest state.
    pub fn read_mem_u8(&self, addr: u32) -> u8 {
        self.mem.read_u8(addr)
    }

    /// Read a 16-bit little-endian word from guest RAM.
    pub fn read_mem_u16(&self, addr: u32) -> u16 {
        self.mem.read_u16(addr)
    }

    /// Capture the VM's state as a compact byte buffer for later
    /// `restore()`. Format v1: 16-byte header (magic + version +
    /// reserved) + 36-byte CPU image + 1 MB memory image. Total
    /// ≈ 1 MiB + 52 bytes.
    ///
    /// **Scope of v1**: CPU and RAM only. Device state (UART buffers,
    /// PIC IMR/IRR/ISR, PIT counter, keyboard queue, CMOS storage)
    /// is *not* preserved — restored snapshots come back with fresh
    /// devices. A snapshot taken mid-handler can therefore land in a
    /// surprising place after restore. Use snapshots when the guest
    /// is at a clean rest point (boot, JMP -2 idle, HLT).
    pub fn snapshot(&self) -> Vec<u8> {
        let total = snapshot::HEADER_LEN + snapshot::CPU_V5_LEN + self.mem.size();
        let mut buf = Vec::with_capacity(total);
        // Header
        buf.extend_from_slice(snapshot::MAGIC);
        buf.push(snapshot::VERSION);
        buf.push(0); // flags (reserved)
        buf.extend_from_slice(&[0u8; 8]); // reserved padding

        // CPU image v1/v2 prefix — preserved verbatim so v3 snapshots
        // remain readable by any tool that knows the v1 layout.
        for r in &self.cpu.regs {
            buf.extend_from_slice(&r.to_le_bytes());
        }
        for s in &self.cpu.sregs {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf.extend_from_slice(&self.cpu.ip.to_le_bytes());
        buf.extend_from_slice(&self.cpu.flags.to_le_bytes());
        buf.push(self.cpu.halted as u8);
        buf.push(match self.cpu.seg_override() {
            None => 0xFF,
            Some(i) => i as u8,
        });
        // 2 reserved bytes — the v1/v2 layout always ended with these.
        buf.extend_from_slice(&[0u8; 2]);

        // v3 CPU extension — upper-16 of each GPR + CR0 + GDTR + IDTR.
        for h in &self.cpu.regs_high {
            buf.extend_from_slice(&h.to_le_bytes());
        }
        buf.extend_from_slice(&self.cpu.cr0.to_le_bytes());
        buf.extend_from_slice(&self.cpu.gdtr.limit.to_le_bytes());
        buf.extend_from_slice(&self.cpu.gdtr.base.to_le_bytes());
        buf.extend_from_slice(&self.cpu.idtr.limit.to_le_bytes());
        buf.extend_from_slice(&self.cpu.idtr.base.to_le_bytes());

        // v4 CPU extension — CR3 (page directory physical base).
        buf.extend_from_slice(&self.cpu.cr3.to_le_bytes());

        // v5 CPU extension — CR2 (last page-fault linear address).
        buf.extend_from_slice(&self.cpu.cr2.to_le_bytes());

        // Memory
        buf.extend_from_slice(self.mem.as_slice());

        // Devices (v2-style length-prefixed records — see IoBus::snapshot).
        let dev = self.io.snapshot();
        let dev_len = dev.len() as u32;
        buf.extend_from_slice(&dev_len.to_le_bytes());
        buf.extend_from_slice(&dev);
        buf
    }

    /// Restore VM state from a buffer produced by `snapshot()`. On
    /// error the VM's state is unchanged (we validate first, mutate
    /// only on success). Devices are *not* restored — they keep
    /// whatever state they had before the call.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), snapshot::SnapshotError> {
        use snapshot::SnapshotError;
        let ram_size = self.mem.size();
        if bytes.len() < snapshot::HEADER_LEN + snapshot::CPU_LEN + ram_size {
            return Err(SnapshotError::TooSmall {
                got: bytes.len(),
                need: snapshot::HEADER_LEN + snapshot::CPU_LEN + ram_size,
            });
        }
        if &bytes[..snapshot::MAGIC.len()] != snapshot::MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = bytes[snapshot::MAGIC.len()];
        if !matches!(version, 1..=5) {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let cpu_len = match version {
            5 => snapshot::CPU_V5_LEN,
            4 => snapshot::CPU_V4_LEN,
            3 => snapshot::CPU_V3_LEN,
            _ => snapshot::CPU_LEN,
        };
        // Re-validate min size against the version-specific CPU image.
        if bytes.len() < snapshot::HEADER_LEN + cpu_len + ram_size {
            return Err(SnapshotError::TooSmall {
                got: bytes.len(),
                need: snapshot::HEADER_LEN + cpu_len + ram_size,
            });
        }
        let cpu_start = snapshot::HEADER_LEN;
        let mem_start = cpu_start + cpu_len;

        // Decode v1/v2 prefix (always present in any version).
        let mut regs = [0u16; 8];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = u16::from_le_bytes([bytes[cpu_start + i * 2], bytes[cpu_start + i * 2 + 1]]);
        }
        let sregs_off = cpu_start + 16;
        let mut sregs = [0u16; 6];
        for (i, s) in sregs.iter_mut().enumerate() {
            *s = u16::from_le_bytes([bytes[sregs_off + i * 2], bytes[sregs_off + i * 2 + 1]]);
        }
        let ip = u16::from_le_bytes([bytes[cpu_start + 28], bytes[cpu_start + 29]]);
        let flags = u16::from_le_bytes([bytes[cpu_start + 30], bytes[cpu_start + 31]]);
        let halted = bytes[cpu_start + 32] != 0;
        let seg_override = match bytes[cpu_start + 33] {
            0xFF => None,
            i if (i as usize) < 6 => Some(i as usize),
            _ => None,
        };

        // v3 extension (regs_high, cr0, gdtr, idtr). For v1/v2 these
        // come back at their defaults (zero / empty).
        let mut regs_high = [0u16; 8];
        let mut cr0: u32 = 0;
        let mut gdtr = wwwvm_cpu::DescriptorTable::default();
        let mut idtr = wwwvm_cpu::DescriptorTable::default();
        let mut cr3: u32 = 0;
        if version >= 3 {
            let ext = cpu_start + snapshot::CPU_LEN;
            for (i, h) in regs_high.iter_mut().enumerate() {
                *h = u16::from_le_bytes([bytes[ext + i * 2], bytes[ext + i * 2 + 1]]);
            }
            cr0 = u32::from_le_bytes([
                bytes[ext + 16],
                bytes[ext + 17],
                bytes[ext + 18],
                bytes[ext + 19],
            ]);
            gdtr.limit = u16::from_le_bytes([bytes[ext + 20], bytes[ext + 21]]);
            gdtr.base = u32::from_le_bytes([
                bytes[ext + 22],
                bytes[ext + 23],
                bytes[ext + 24],
                bytes[ext + 25],
            ]);
            idtr.limit = u16::from_le_bytes([bytes[ext + 26], bytes[ext + 27]]);
            idtr.base = u32::from_le_bytes([
                bytes[ext + 28],
                bytes[ext + 29],
                bytes[ext + 30],
                bytes[ext + 31],
            ]);
        }
        if version >= 4 {
            let ext = cpu_start + snapshot::CPU_V3_LEN;
            cr3 = u32::from_le_bytes([bytes[ext], bytes[ext + 1], bytes[ext + 2], bytes[ext + 3]]);
        }
        let mut cr2: u32 = 0;
        if version >= 5 {
            let ext = cpu_start + snapshot::CPU_V4_LEN;
            cr2 = u32::from_le_bytes([bytes[ext], bytes[ext + 1], bytes[ext + 2], bytes[ext + 3]]);
        }

        // Memory restore — `restore_full` validates size again as a
        // defense-in-depth check, but we already verified above.
        self.mem
            .restore_full(&bytes[mem_start..mem_start + ram_size])
            .map_err(|expected| SnapshotError::MemorySizeMismatch {
                expected,
                actual: bytes.len() - mem_start,
            })?;
        // Device section (v2 and v3). v1 snapshots have nothing here.
        if version >= 2 {
            let dev_off = mem_start + ram_size;
            if bytes.len() < dev_off + 4 {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: dev_off + 4,
                });
            }
            let dev_len = u32::from_le_bytes([
                bytes[dev_off],
                bytes[dev_off + 1],
                bytes[dev_off + 2],
                bytes[dev_off + 3],
            ]) as usize;
            if bytes.len() < dev_off + 4 + dev_len {
                return Err(SnapshotError::TooSmall {
                    got: bytes.len(),
                    need: dev_off + 4 + dev_len,
                });
            }
            self.io
                .restore(&bytes[dev_off + 4..dev_off + 4 + dev_len])
                .map_err(SnapshotError::DeviceRestore)?;
        }

        // Commit CPU state.
        self.cpu.regs = regs;
        self.cpu.regs_high = regs_high;
        self.cpu.sregs = sregs;
        self.cpu.ip = ip;
        self.cpu.flags = flags;
        self.cpu.halted = halted;
        self.cpu.set_seg_override(seg_override);
        self.cpu.cr0 = cr0;
        self.cpu.gdtr = gdtr;
        self.cpu.idtr = idtr;
        self.cpu.cr3 = cr3;
        self.cpu.cr2 = cr2;
        // Re-derive seg_cache from the visible selectors. For real-
        // mode snapshots this is exact (cache = sel << 4). For a
        // future PM snapshot the cache values would diverge from
        // sel<<4 and need their own section in a v4 layout.
        for (slot, sel) in self.cpu.seg_cache.iter_mut().zip(sregs.iter()) {
            *slot = wwwvm_cpu::SegmentCache {
                base: (*sel as u32) << 4,
                limit: 0xFFFF,
                access: 0x93,
            };
        }
        self.booted = true;
        Ok(())
    }

    /// Snapshot the VGA text-mode buffer as a plain string: 25 rows
    /// of 80 chars, newline-separated. The attribute byte of each
    /// cell is dropped; control bytes (and NULs from un-initialized
    /// buffer) become spaces so the result is always readable. Use
    /// this to render the guest's text-mode display in the host UI
    /// or to assert on guest output in tests.
    pub fn vga_text_snapshot(&self) -> String {
        let mut out = String::with_capacity(VGA_TEXT_ROWS * (VGA_TEXT_COLS + 1));
        for row in 0..VGA_TEXT_ROWS {
            for col in 0..VGA_TEXT_COLS {
                let off = ((row * VGA_TEXT_COLS) + col) * 2;
                let ch = self.mem.read_u8(VGA_TEXT_BASE + off as u32);
                if (0x20..0x7F).contains(&ch) {
                    out.push(ch as char);
                } else {
                    out.push(' ');
                }
            }
            out.push('\n');
        }
        out
    }

    /// Load the bundled hello guest at the standard boot-sector address.
    pub fn load_default_guest(&mut self) {
        self.load_image(BOOT_LOAD_ADDR, HELLO_GUEST);
    }

    /// Load the bundled interactive demo: an interrupt-driven UART
    /// echo with a banner. Installs main + handler + greeting in
    /// memory and wires the IRQ-4 vector through the IVT — JS just
    /// has to call `boot()` afterwards.
    pub fn load_interactive_demo(&mut self) {
        use interactive_demo as d;
        self.load_image(d::MAIN_ADDR, d::MAIN);
        self.load_image(d::HANDLER_ADDR, d::HANDLER);
        self.load_image(d::GREETING_ADDR, d::GREETING);
        self.set_ivt(d::IRQ4_VECTOR, 0, d::HANDLER_ADDR as u16);
    }

    /// Load the bundled mini-calculator demo: poll a byte, square it
    /// via `MUL`, print the result as decimal followed by `\n`. Lives
    /// across two memory regions — main at 0x7C00, the print_dec
    /// subroutine at 0x7C30.
    pub fn load_calculator_demo(&mut self) {
        use calculator_demo as d;
        self.load_image(d::BASE_ADDR, d::MAIN);
        self.load_image(d::PRINT_DEC_ADDR, d::PRINT_DEC);
    }

    /// Queue commands to be delivered to the guest the moment it boots.
    /// Each command is terminated with `\n` and they are concatenated in
    /// order — `["ls", "cat /etc/os-release"]` becomes `"ls\ncat …\n"`.
    pub fn set_autorun_commands<I, S>(&mut self, commands: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.autorun.clear();
        for cmd in commands {
            self.autorun.extend_from_slice(cmd.as_ref().as_bytes());
            self.autorun.push(b'\n');
        }
    }

    /// Reset the CPU to the boot state and deliver autorun bytes (if any)
    /// to the UART input buffer so the guest reads them first.
    pub fn boot(&mut self) {
        self.cpu.reset_to_boot();
        self.io.uart_mut().push_rx(&self.autorun);
        self.autorun.clear();
        self.booted = true;
    }

    /// Wire the host-side BIOS shim into the CPU. After this, `INT 0x10`
    /// (and future BIOS vectors) gets dispatched to the Rust functions
    /// in [`bios_hook`] instead of the IVT entry — so a freshly booted
    /// guest can print via teletype without first installing its own
    /// handler. A guest that *does* install its own IVT entry can
    /// override us by overwriting the `bios_hook` field with `None`.
    pub fn install_bios(&mut self) {
        self.cpu.bios_hook = Some(bios_hook);
    }

    /// Replace the boot disk image. The host-side `INT 0x13 AH=0x02`
    /// shim reads from here. `bytes.len()` need not be a sector
    /// multiple; bytes past the end read as zero.
    pub fn load_disk_image(&mut self, bytes: &[u8]) {
        self.io.disk.load(bytes);
    }

    /// Parse an ELF32 image and copy its PT_LOAD segments into guest
    /// memory. After this returns, the caller should call [`boot`]
    /// (which resets the CPU) and then position CS:IP at the entry
    /// point — typically by writing the IP register directly. The
    /// loader doesn't touch CS:IP itself because the caller might
    /// want to construct a different initial state (e.g. PM-up
    /// already, GDT pre-installed) before jumping to entry.
    pub fn load_elf_image(&mut self, bytes: &[u8]) -> Result<u32, ElfError> {
        elf::load_elf(&mut self.mem, bytes)
    }

    /// Parse a Linux bzImage and lay it out at the canonical addresses:
    /// the setup blob (boot-sector + setup sectors, `payload_offset`
    /// bytes total) lands at linear 0x90000 — the historical "SETUPSEG"
    /// = 0x9000:0000 — and the protected-mode kernel payload lands at
    /// `code32_start` from the header (typically 0x10_0000).
    ///
    /// Returns the parsed [`BzImage`] so the caller can read fields it
    /// also needs (entry, ramdisk pointers, etc.).
    ///
    /// Loader-only: this does *not* boot the kernel — see the bzImage
    /// integration test for the rest of the dance. A future tick will
    /// add an end-to-end `boot_bzimage` once we have the matching
    /// real-mode setup execution flow.
    pub fn load_bzimage(&mut self, bytes: &[u8]) -> Result<BzImage, BzImageError> {
        let bz = bzimage::parse(bytes)?;
        // Setup blob at linear 0x90000.
        self.mem.write_slice(0x9_0000, &bytes[..bz.payload_offset]);
        // 32-bit payload at code32_start.
        if bz.payload_offset < bytes.len() {
            self.mem
                .write_slice(bz.code32_start, &bytes[bz.payload_offset..]);
        }
        Ok(bz)
    }

    /// Cold-boot from disk: reset the CPU, copy sector 0 of the loaded
    /// disk image to linear `0x7C00` (the standard boot-sector load
    /// address), then continue with the same autorun/UART setup as
    /// [`boot`]. Real BIOS does this before the IVT is even guest-
    /// visible — we model it directly on `Vm` rather than inside the
    /// CPU because nothing in CPU state needs to know.
    pub fn boot_from_disk(&mut self) {
        self.cpu.reset_to_boot();
        let mut buf = [0u8; wwwvm_devices::DISK_SECTOR_SIZE];
        self.io.disk.read_sectors(0, 1, &mut buf);
        self.mem.write_slice(BOOT_LOAD_ADDR, &buf);
        self.io.uart_mut().push_rx(&self.autorun);
        self.autorun.clear();
        self.booted = true;
    }

    pub fn is_booted(&self) -> bool {
        self.booted
    }

    pub fn is_halted(&self) -> bool {
        self.cpu.halted
    }

    /// Push bytes for the guest to read via the UART. JS uses this to
    /// forward keystrokes or `runCommand` payloads.
    pub fn send_input(&mut self, bytes: &[u8]) {
        self.io.uart_mut().push_rx(bytes);
    }

    /// Push a raw scan code byte into the PS/2 keyboard buffer.
    /// Raises IRQ 1 to a guest that has unmasked it. The translation
    /// from host keystrokes to scan codes (Set 1 / Set 2) is the
    /// host's job — this just queues bytes.
    pub fn push_scancode(&mut self, code: u8) {
        self.io.kbd.push_scancode(code);
    }

    /// Seed the CMOS clock with binary date/time. Year is two-digit
    /// (00..99). A guest probing 0x70/0x71 sees these values, with
    /// Status B already configured for binary + 24-hour mode.
    pub fn set_cmos_time(
        &mut self,
        year: u8,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) {
        self.io
            .cmos
            .set_time(year, month, day, hour, minute, second);
    }

    /// Drain everything the guest has transmitted since the last call.
    pub fn drain_output(&mut self) -> Vec<u8> {
        self.io.uart_mut().drain_tx()
    }

    /// Step the CPU up to `max` times. Returns (steps_executed, reason).
    pub fn run_steps(&mut self, max: u32) -> (u32, Stop) {
        if !self.booted {
            self.boot();
        }
        let mut executed = 0;
        for _ in 0..max {
            if self.cpu.halted {
                return (executed, Stop::Halted);
            }
            if let Err(e) = self.cpu.step(&mut self.mem, &mut self.io) {
                return (executed, Stop::CpuError(e));
            }
            executed += 1;
        }
        (executed, Stop::StepBudget)
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }
    pub fn mem(&self) -> &Memory {
        &self.mem
    }
}

/// Hand-assembled real-mode guest. Layout:
///
/// ```text
/// 00: BE 1D 7C       mov si, 0x7C1D      ; -> "wwwvm: ready\n\0"
/// 03: AC             lodsb               ; AL = DS:[SI], SI++
/// 04: 08 C0          or al, al           ; ZF=1 if NUL
/// 06: 74 06          jz  +6  -> 0x0E     ; into read loop on NUL
/// 08: BA F8 03       mov dx, 0x3F8       ; UART THR
/// 0B: EE             out dx, al
/// 0C: EB F5          jmp -11 -> 0x03     ; next char
/// 0E: BA FD 03       mov dx, 0x3FD       ; UART LSR  (read loop)
/// 11: EC             in  al, dx
/// 12: A8 01          test al, 1          ; DR bit
/// 14: 74 F8          jz  -8  -> 0x0E
/// 16: BA F8 03       mov dx, 0x3F8       ; UART RBR
/// 19: EC             in  al, dx
/// 1A: EE             out dx, al          ; echo
/// 1B: EB F1          jmp -15 -> 0x0E
/// 1D: "wwwvm: ready\n\0"
/// ```
pub const HELLO_GUEST: &[u8] = &[
    0xBE, 0x1D, 0x7C, // mov si, 0x7C1D
    0xAC, // lodsb
    0x08, 0xC0, // or al, al
    0x74, 0x06, // jz +6
    0xBA, 0xF8, 0x03, // mov dx, 0x3F8
    0xEE, // out dx, al
    0xEB, 0xF5, // jmp -11
    0xBA, 0xFD, 0x03, // mov dx, 0x3FD
    0xEC, // in al, dx
    0xA8, 0x01, // test al, 1
    0x74, 0xF8, // jz -8
    0xBA, 0xF8, 0x03, // mov dx, 0x3F8
    0xEC, // in al, dx
    0xEE, // out dx, al
    0xEB, 0xF1, // jmp -15
    b'w', b'w', b'w', b'v', b'm', b':', b' ', b'r', b'e', b'a', b'd', b'y', 0x0A, 0x00,
];

/// Mini-calculator guest. Each byte pushed via `vm.send_input(&[n])`
/// is squared with `MUL r/m8` (AX = AL × BL) and printed as decimal
/// followed by a newline. The print_dec subroutine showcases the
/// canonical ASCII-formatting idiom — divide by ten, push the
/// remainder, repeat until the quotient is zero, then pop and emit.
///
/// `[7]` → `"49\n"`, `[16]` → `"256\n"`, `[255]` → `"65025\n"`.
pub mod calculator_demo {
    pub const BASE_ADDR: u32 = 0x7C00;
    pub const PRINT_DEC_ADDR: u32 = 0x7C30;

    /// Main poll-mul-print loop. See [docs/HAND_ASSEMBLY.md](../../docs/HAND_ASSEMBLY.md)
    /// for an annotated walkthrough of how the bytes encode the
    /// instructions.
    pub const MAIN: &[u8] = &[
        // 0x00 BA FD 03    MOV DX, 0x3FD       ; UART LSR
        0xBA, 0xFD, 0x03, // 0x03 EC          IN  AL, DX
        0xEC, // 0x04 A8 01       TEST AL, 1
        0xA8, 0x01, // 0x06 74 FB       JZ  -5  -> 0x03     ; spin until ready
        0x74, 0xFB, // 0x08 BA F8 03    MOV DX, 0x3F8       ; UART RBR
        0xBA, 0xF8, 0x03, // 0x0B EC          IN  AL, DX
        0xEC, // 0x0C 88 C3       MOV BL, AL
        0x88, 0xC3, // 0x0E F6 E3       MUL BL              ; AX = AL * BL
        0xF6, 0xE3, // 0x10 E8 1D 00    CALL +0x1D -> 0x30  ; print_dec
        0xE8, 0x1D, 0x00, // 0x13 B0 0A       MOV AL, '\n'
        0xB0, 0x0A, // 0x15 BA F8 03    MOV DX, 0x3F8
        0xBA, 0xF8, 0x03, // 0x18 EE          OUT DX, AL
        0xEE, // 0x19 EB E5       JMP -27 -> 0x00     ; next input
        0xEB, 0xE5,
    ];

    /// `print_dec` — emits AX as a variable-length decimal string.
    /// Loaded at 0x7C30 so the main routine's `CALL +0x1D` lands on
    /// the first instruction.
    pub const PRINT_DEC: &[u8] = &[
        // 0x30 BB 0A 00    MOV BX, 10
        0xBB, 0x0A, 0x00, // 0x33 31 C9       XOR CX, CX
        0x31, 0xC9, // 0x35 31 D2       XOR DX, DX
        0x31, 0xD2, // 0x37 F7 F3       DIV BX
        0xF7, 0xF3, // 0x39 52          PUSH DX
        0x52, // 0x3A 41          INC CX
        0x41, // 0x3B 09 C0       OR  AX, AX
        0x09, 0xC0, // 0x3D 75 F6       JNZ -10 -> 0x35
        0x75, 0xF6, // 0x3F 58          POP AX
        0x58, // 0x40 04 30       ADD AL, '0'
        0x04, 0x30, // 0x42 BA F8 03    MOV DX, 0x3F8
        0xBA, 0xF8, 0x03, // 0x45 EE          OUT DX, AL
        0xEE, // 0x46 E2 F7       LOOP -9 -> 0x3F
        0xE2, 0xF7, // 0x48 C3          RET
        0xC3,
    ];
}

/// Interrupt-driven interactive demo. Unlike [`HELLO_GUEST`], which
/// polls the UART LSR in a tight loop, this one wires the UART to
/// IRQ 4 and lets the CPU spin in `JMP -2` — characters typed by the
/// host are delivered to the guest via an interrupt and echoed back
/// from the handler. Demonstrates the IDT + PIC + UART pipeline on
/// the same `Vm` API used by JS.
pub mod interactive_demo {
    pub const MAIN_ADDR: u32 = 0x7C00;
    pub const HANDLER_ADDR: u32 = 0x7C30;
    pub const GREETING_ADDR: u32 = 0x7C50;
    /// COM1 → IRQ 4 → vector 0x0C with the default PIC vector base 8.
    pub const IRQ4_VECTOR: u8 = 0x0C;

    /// Main entry. Prints the greeting via LODSB+OUT, configures
    /// UART IER, unmasks IRQ 4, STIs, then sits in an infinite
    /// `JMP -2` so refresh_irqs keeps polling between interrupts.
    ///
    /// ```text
    /// 0x00 BE 50 7C    MOV SI, 0x7C50              ; greeting string
    /// 0x03 AC          LODSB
    /// 0x04 08 C0       OR  AL, AL
    /// 0x06 74 06       JZ  +6  -> 0x0E (banner done)
    /// 0x08 BA F8 03    MOV DX, 0x3F8
    /// 0x0B EE          OUT DX, AL
    /// 0x0C EB F5       JMP -11 -> 0x03 (next char)
    /// 0x0E BA F9 03    MOV DX, 0x3F9               ; UART IER
    /// 0x11 B0 01       MOV AL, 1
    /// 0x13 EE          OUT DX, AL
    /// 0x14 B0 EF       MOV AL, 0xEF                ; unmask IRQ 4 only
    /// 0x16 E6 21       OUT 0x21, AL                ; PIC IMR
    /// 0x18 FB          STI
    /// 0x19 EB FE       JMP -2                       ; spin
    /// ```
    pub const MAIN: &[u8] = &[
        0xBE, 0x50, 0x7C, 0xAC, 0x08, 0xC0, 0x74, 0x06, 0xBA, 0xF8, 0x03, 0xEE, 0xEB, 0xF5, 0xBA,
        0xF9, 0x03, 0xB0, 0x01, 0xEE, 0xB0, 0xEF, 0xE6, 0x21, 0xFB, 0xEB, 0xFE,
    ];

    /// UART IRQ 4 handler. Reads RBR into AL, writes it straight back
    /// to THR (echo), EOIs the PIC, IRETs.
    ///
    /// ```text
    /// 0x00 50          PUSH AX
    /// 0x01 52          PUSH DX
    /// 0x02 BA F8 03    MOV DX, 0x3F8
    /// 0x05 EC          IN  AL, DX
    /// 0x06 EE          OUT DX, AL                  ; echo
    /// 0x07 B0 20       MOV AL, 0x20
    /// 0x09 E6 20       OUT 0x20, AL                ; non-specific EOI
    /// 0x0B 5A          POP DX
    /// 0x0C 58          POP AX
    /// 0x0D CF          IRET
    /// ```
    pub const HANDLER: &[u8] = &[
        0x50, 0x52, 0xBA, 0xF8, 0x03, 0xEC, 0xEE, 0xB0, 0x20, 0xE6, 0x20, 0x5A, 0x58, 0xCF,
    ];

    /// NUL-terminated banner printed once on boot. The trailing newline
    /// matters: terminals only flush a line when they see `\n`.
    pub const GREETING: &[u8] = b"wwwvm interactive\n\0";
}

#[cfg(test)]
mod tests;
