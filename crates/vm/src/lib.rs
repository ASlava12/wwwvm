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

    /// Copy bytes into physical RAM at `addr`.
    pub fn load_image(&mut self, addr: u32, bytes: &[u8]) {
        self.mem.write_slice(addr, bytes);
    }

    /// Load the bundled hello guest at the standard boot-sector address.
    pub fn load_default_guest(&mut self) {
        self.load_image(BOOT_LOAD_ADDR, HELLO_GUEST);
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

    pub fn cpu(&self) -> &Cpu { &self.cpu }
    pub fn mem(&self) -> &Memory { &self.mem }
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
    0xBE, 0x1D, 0x7C,                   // mov si, 0x7C1D
    0xAC,                                // lodsb
    0x08, 0xC0,                          // or al, al
    0x74, 0x06,                          // jz +6
    0xBA, 0xF8, 0x03,                    // mov dx, 0x3F8
    0xEE,                                // out dx, al
    0xEB, 0xF5,                          // jmp -11
    0xBA, 0xFD, 0x03,                    // mov dx, 0x3FD
    0xEC,                                // in al, dx
    0xA8, 0x01,                          // test al, 1
    0x74, 0xF8,                          // jz -8
    0xBA, 0xF8, 0x03,                    // mov dx, 0x3F8
    0xEC,                                // in al, dx
    0xEE,                                // out dx, al
    0xEB, 0xF1,                          // jmp -15
    b'w', b'w', b'w', b'v', b'm', b':', b' ',
    b'r', b'e', b'a', b'd', b'y', 0x0A, 0x00,
];

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
            0xB9, 0x05, 0x00,
            0xB0, 0x41,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xFE, 0xC0,
            0xE2, 0xFB,
            0xF4,
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
            0xBA, 0xFD, 0x03,
            0xEC,
            0xA8, 0x01,
            0x74, 0xF8,
            0xBA, 0xF8, 0x03,
            0xEC,
            0x88, 0xC3,
            0xF6, 0xE3,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xEB, 0xEA,
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

    /// End-to-end interrupt-driven serial: guest sets up an IRQ 4
    /// handler that reads a byte from the UART RBR into BL, EOIs the
    /// PIC, and IRETs. The main routine enables IER bit 0, unmasks
    /// IRQ 4, STIs, and spins until BL != 0 then HLTs.
    ///
    /// The host pushes a byte to the UART rx queue before stepping;
    /// the very first `refresh_irqs` should latch IRQ 4 into the PIC,
    /// the CPU dispatches to the handler, the handler drains the
    /// byte, and the main loop exits.
    ///
    /// Layout:
    /// ```text
    /// 0x00: C7 06 30 00 20 7C    MOV WORD [0x30], 0x7C20   ; IVT[0x0C].offset
    /// 0x06: C7 06 32 00 00 00    MOV WORD [0x32], 0x0000   ; IVT[0x0C].segment
    /// 0x0C: FB                    STI
    /// 0x0D: BA F9 03              MOV DX, 0x3F9             ; UART IER
    /// 0x10: B0 01                 MOV AL, 1
    /// 0x12: EE                    OUT DX, AL
    /// 0x13: B0 EF                 MOV AL, 0xEF
    /// 0x15: E6 21                 OUT 0x21, AL              ; PIC IMR
    /// 0x17: 80 FB 00              CMP BL, 0
    /// 0x1A: 74 FB                 JZ -5 -> 0x17
    /// 0x1C: F4                    HLT
    /// (padding to 0x20)
    /// 0x20: 50                    PUSH AX                   ; handler
    /// 0x21: BA F8 03              MOV DX, 0x3F8             ; UART RBR
    /// 0x24: EC                    IN AL, DX
    /// 0x25: 88 C3                 MOV BL, AL
    /// 0x27: B0 20                 MOV AL, 0x20
    /// 0x29: E6 20                 OUT 0x20, AL              ; PIC EOI
    /// 0x2B: 58                    POP AX
    /// 0x2C: CF                    IRET
    /// ```
    #[test]
    fn uart_rx_drives_irq4_handler_through_vm() {
        let program: &[u8] = &[
            // setup IVT[0x0C]
            0xC7, 0x06, 0x30, 0x00, 0x20, 0x7C,
            0xC7, 0x06, 0x32, 0x00, 0x00, 0x00,
            // enable IF
            0xFB,
            // IER = 1
            0xBA, 0xF9, 0x03,
            0xB0, 0x01,
            0xEE,
            // unmask IRQ 4
            0xB0, 0xEF,
            0xE6, 0x21,
            // wait-loop: while BL == 0 spin
            0x80, 0xFB, 0x00,
            0x74, 0xFB,
            0xF4,
            // padding to 0x20
            0x90, 0x90, 0x90,
            // handler at 0x20
            0x50,
            0xBA, 0xF8, 0x03,
            0xEC,
            0x88, 0xC3,
            0xB0, 0x20,
            0xE6, 0x20,
            0x58,
            0xCF,
        ];
        let mut vm = Vm::new();
        vm.load_image(BOOT_LOAD_ADDR, program);
        vm.boot();
        // Push the byte the guest is supposed to pick up via IRQ.
        vm.send_input(&[0x42]);
        let (_, stop) = vm.run_steps(5_000);
        match stop {
            Stop::Halted => {}
            other => panic!("expected Halted, got {other:?}"),
        }
        match stop {
            Stop::Halted => {}
            other => panic!("expected Halted, got {other:?}"),
        }
        // BL was set by the handler reading from RBR.
        assert_eq!(vm.cpu().regs[wwwvm_cpu::r16::BX] & 0xFF, 0x42);
    }

    /// Divide-by-zero in the guest must surface as `Stop::CpuError`
    /// rather than silently producing garbage. This is the VM-side
    /// view of `CpuError::DivideError`.
    #[test]
    fn div_by_zero_surfaces_through_vm_stop() {
        // MOV AL, 5 ; MOV BL, 0 ; DIV BL ; HLT (unreached)
        let program: &[u8] = &[
            0xB0, 0x05,
            0xB3, 0x00,
            0xF6, 0xF3,
            0xF4,
        ];
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
