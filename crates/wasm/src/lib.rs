//! wasm-bindgen surface for the browser.
//!
//! JS sees a single `WwwVm` class. The intended pump pattern is:
//!
//! ```js
//! const vm = new WwwVm();
//! vm.load_default_guest();
//! vm.set_autorun(["uname -a", "ls /"]);
//! vm.boot();
//! function tick() {
//!     vm.run(50_000);
//!     const out = vm.read_output();
//!     if (out) term.write(out);
//!     requestAnimationFrame(tick);
//! }
//! tick();
//!
//! // anytime:
//! vm.send_command("date");
//! ```
//!
//! Errors from the CPU are surfaced as JS exceptions via `Result<…, JsError>`.

#![forbid(unsafe_code)]

use wasm_bindgen::prelude::*;
use wwwvm_vm::{Stop, Vm};

#[wasm_bindgen]
pub struct WwwVm {
    inner: Vm,
    last_error: Option<String>,
}

#[wasm_bindgen]
impl WwwVm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self { inner: Vm::new(), last_error: None }
    }

    /// Load the bundled hello guest at the standard boot-sector address.
    pub fn load_default_guest(&mut self) {
        self.inner.load_default_guest();
    }

    /// Load the interrupt-driven interactive demo (banner + UART
    /// echo through IRQ 4). Sets up the IVT itself.
    pub fn load_interactive_demo(&mut self) {
        self.inner.load_interactive_demo();
    }

    /// Load arbitrary bytes (e.g. a future kernel/initrd) at `addr`.
    pub fn load_image(&mut self, addr: u32, bytes: &[u8]) {
        self.inner.load_image(addr, bytes);
    }

    /// Write an IVT entry. Vector `v` lives at linear `v*4`. Use this
    /// from JS to install an interrupt handler without emitting MOV
    /// WORD instructions in the guest.
    pub fn set_ivt(&mut self, vector: u8, segment: u16, offset: u16) {
        self.inner.set_ivt(vector, segment, offset);
    }

    /// Read a single byte from guest RAM.
    pub fn read_mem_u8(&self, addr: u32) -> u8 {
        self.inner.read_mem_u8(addr)
    }

    /// Read a 16-bit little-endian word from guest RAM.
    pub fn read_mem_u16(&self, addr: u32) -> u16 {
        self.inner.read_mem_u16(addr)
    }

    /// Snapshot the VGA text-mode buffer as 25 newline-separated rows
    /// of 80 ASCII characters. Attribute bytes are dropped. Useful
    /// for rendering the guest's text-mode display alongside the
    /// UART terminal in the host UI.
    pub fn vga_text_snapshot(&self) -> String {
        self.inner.vga_text_snapshot()
    }

    /// Pre-queue commands to be delivered to the guest at boot. Pass an
    /// array of strings from JS — each is appended with `\n`.
    pub fn set_autorun(&mut self, commands: Vec<String>) {
        self.inner.set_autorun_commands(commands.iter());
    }

    /// Reset the CPU and prime autorun bytes. Safe to call multiple
    /// times — each call is a fresh boot.
    pub fn boot(&mut self) {
        self.last_error = None;
        self.inner.boot();
    }

    /// Step the CPU up to `cycles` times. Returns the number of steps
    /// actually executed. If the CPU hits an unimplemented opcode, the
    /// error is stashed (see `last_error`) and the function returns
    /// however many steps ran before the failure.
    pub fn run(&mut self, cycles: u32) -> u32 {
        let (steps, stop) = self.inner.run_steps(cycles);
        if let Stop::CpuError(e) = stop {
            self.last_error = Some(e.to_string());
        }
        steps
    }

    /// True if the CPU is parked on HLT.
    pub fn is_halted(&self) -> bool { self.inner.is_halted() }

    /// True if `boot()` has been called at least once.
    pub fn is_booted(&self) -> bool { self.inner.is_booted() }

    /// Last CPU error message (e.g. "unimplemented opcode 0x0F at
    /// 0000:7C20"), or null if the run loop has not failed.
    #[wasm_bindgen(getter)]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    /// Push a raw command string into the guest's stdin, terminating
    /// with `\n`. Used for `runCommand` from JS.
    pub fn send_command(&mut self, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        self.inner.send_input(&bytes);
    }

    /// Push raw bytes (no newline added) — useful for sending individual
    /// keystrokes from a terminal widget.
    pub fn send_input(&mut self, bytes: &[u8]) {
        self.inner.send_input(bytes);
    }

    /// Push a raw scan-code byte into the PS/2 keyboard buffer. Used
    /// for guests that expect IRQ 1 + port 0x60 instead of the UART
    /// byte stream. Host is responsible for translating from a
    /// keyboard event to the scan code the guest expects (Set 1, Set
    /// 2, etc.).
    pub fn push_scancode(&mut self, code: u8) {
        self.inner.push_scancode(code);
    }

    /// Seed the CMOS clock with binary date/time. Year is two-digit
    /// (00..99). Useful from JS when you want the guest to read a
    /// specific wall-clock time on boot.
    pub fn set_cmos_time(
        &mut self,
        year: u8,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) {
        self.inner.set_cmos_time(year, month, day, hour, minute, second);
    }

    /// Drain everything the guest has emitted since the previous call.
    /// Returned as a UTF-8 string with lossy replacement for non-UTF-8
    /// bytes (the host UART is a byte stream, not text — but the demo
    /// terminal expects text).
    pub fn read_output(&mut self) -> String {
        let bytes = self.inner.drain_output();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Default for WwwVm {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // wasm-bindgen attributes are inert on the host target, so we can
    // exercise the wrapper directly.

    #[test]
    fn end_to_end_run_command_loop() {
        let mut vm = WwwVm::new();
        vm.load_default_guest();
        vm.set_autorun(vec!["hello".into()]);
        vm.boot();
        vm.run(5_000);
        let out = vm.read_output();
        assert!(out.contains("wwwvm: ready"));
        assert!(out.contains("hello\n"));
        assert!(vm.last_error().is_none());

        vm.send_command("ping");
        vm.run(2_000);
        let out = vm.read_output();
        assert!(out.contains("ping\n"));
    }
}
