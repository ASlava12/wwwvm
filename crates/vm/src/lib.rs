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
}
