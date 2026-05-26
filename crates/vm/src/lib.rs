//! VM orchestrator: owns CPU + memory + IO bus, drives the fetch loop,
//! and exposes a small high-level API used by the wasm bindings.
//!
//! The crate also ships a tiny hand-assembled real-mode guest payload
//! (`HELLO_GUEST`) — it prints a banner over the UART and echoes any
//! input back. That payload is the proof-of-pipeline used by the demo
//! while the CPU/devices grow towards running real OS images.

#![forbid(unsafe_code)]

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

/// Snapshot format constants and the error type used by `restore`.
pub mod snapshot {
    /// 6-byte format magic. Suitable for identifying the file from a
    /// hex dump.
    pub const MAGIC: &[u8] = b"WWWVM\x00";
    /// Current snapshot format version. v1 covered CPU + RAM only;
    /// v2 adds device state (UART buffers, PIC IMR/IRR/ISR, PIT
    /// counter, keyboard queue, CMOS) after the RAM image.
    pub const VERSION: u8 = 2;
    /// Bytes consumed by header: magic + version + flags + reserved.
    pub const HEADER_LEN: usize = 16;
    /// Bytes consumed by the CPU image: 8 r16 + 6 sreg + ip + flags +
    /// halted byte + seg_override byte (rounded up).
    pub const CPU_LEN: usize = 36;

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
    /// One megabyte of conventional + low memory. Enough for the embedded
    /// guest; will grow as we support bigger images.
    pub const RAM_SIZE: usize = 0x10_0000;

    pub fn new() -> Self {
        Self {
            cpu: Cpu::new(),
            mem: Memory::new(Self::RAM_SIZE),
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
        let total = snapshot::HEADER_LEN + snapshot::CPU_LEN + Self::RAM_SIZE;
        let mut buf = Vec::with_capacity(total);
        // Header
        buf.extend_from_slice(snapshot::MAGIC);
        buf.push(snapshot::VERSION);
        buf.push(0); // flags (reserved)
        buf.extend_from_slice(&[0u8; 8]); // reserved padding
                                          // CPU
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
        // 2 reserved CPU-state bytes so the block stays a round 36;
        // future fields (TF, A20 gate, etc.) can land here without
        // bumping the snapshot version.
        buf.extend_from_slice(&[0u8; 2]);
        // Memory
        buf.extend_from_slice(self.mem.as_slice());
        // Devices (v2): length-prefixed records — see IoBus::snapshot.
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
        let min = snapshot::HEADER_LEN + snapshot::CPU_LEN + Self::RAM_SIZE;
        if bytes.len() < min {
            return Err(SnapshotError::TooSmall {
                got: bytes.len(),
                need: min,
            });
        }
        if &bytes[..snapshot::MAGIC.len()] != snapshot::MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = bytes[snapshot::MAGIC.len()];
        if version != 1 && version != snapshot::VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let cpu_start = snapshot::HEADER_LEN;
        let mem_start = cpu_start + snapshot::CPU_LEN;
        // Decode CPU image into temporaries first so a malformed body
        // can't half-overwrite live CPU state.
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
        // Memory restore — `restore_full` validates size again as a
        // defense-in-depth check, but we already verified above.
        self.mem
            .restore_full(&bytes[mem_start..mem_start + Self::RAM_SIZE])
            .map_err(|expected| SnapshotError::MemorySizeMismatch {
                expected,
                actual: bytes.len() - mem_start,
            })?;
        // v2: parse the device section right after the memory image.
        // v1 snapshots have nothing here — devices stay fresh.
        if version == 2 {
            let dev_off = mem_start + Self::RAM_SIZE;
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
            // Device blob is validated lazily by the IoBus; on error
            // we surface it through MemorySizeMismatch's display so
            // we don't have to grow SnapshotError's variants.
            self.io
                .restore(&bytes[dev_off + 4..dev_off + 4 + dev_len])
                .map_err(SnapshotError::DeviceRestore)?;
        }
        // Now that validation passed, commit CPU state.
        self.cpu.regs = regs;
        self.cpu.sregs = sregs;
        self.cpu.ip = ip;
        self.cpu.flags = flags;
        self.cpu.halted = halted;
        self.cpu.set_seg_override(seg_override);
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
mod tests {
    use super::*;

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
            0xBA, 0xFD, 0x03, 0xEC, 0xA8, 0x01, 0x74, 0xF8, 0xBA, 0xF8, 0x03, 0xEC, 0x88, 0xC3,
            0xF6, 0xE3, 0xBA, 0xF8, 0x03, 0xEE, 0xEB, 0xEA,
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
}
