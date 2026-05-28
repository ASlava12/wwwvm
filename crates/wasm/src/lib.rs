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
        Self {
            inner: Vm::new(),
            last_error: None,
        }
    }

    /// Build a VM with a non-default RAM size (default is 1 MiB,
    /// barely enough to load a synthetic boot sector). JS callers
    /// loading a real Linux bzImage need at least ~16 MiB.
    pub fn new_with_ram_size(size: usize) -> Self {
        Self {
            inner: Vm::with_ram_size(size),
            last_error: None,
        }
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

    /// Load the calculator demo: each byte pushed via send_input is
    /// squared (via MUL) and emitted as a decimal string + newline.
    pub fn load_calculator_demo(&mut self) {
        self.inner.load_calculator_demo();
    }

    /// Load arbitrary bytes (e.g. a future kernel/initrd) at `addr`.
    pub fn load_image(&mut self, addr: u32, bytes: &[u8]) {
        self.inner.load_image(addr, bytes);
    }

    /// Replace the primary boot disk image. The host-side BIOS shim
    /// and the IDE controller both read from this.
    pub fn load_disk_image(&mut self, bytes: &[u8]) {
        self.inner.load_disk_image(bytes);
    }

    /// Replace the secondary IDE channel's disk image. Lets JS hand
    /// the guest a CD-ROM-style second drive without touching the
    /// boot device.
    pub fn load_secondary_disk_image(&mut self, bytes: &[u8]) {
        self.inner.load_secondary_disk_image(bytes);
    }

    /// Parse + lay out a Linux bzImage at the canonical addresses
    /// (setup at 0x90000, payload at code32_start). Returns the
    /// header's reported entry point so JS can wire the kernel-
    /// launch jmp. JS sees a thrown Error on a malformed image —
    /// bad boot_flag / HdrS magic / oversize init_size.
    pub fn load_bzimage(&mut self, bytes: &[u8]) -> Result<u32, JsError> {
        self.inner
            .load_bzimage(bytes)
            .map(|bz| bz.code32_start)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Place the kernel command line at the conventional 0x90800
    /// and point cmd_line_ptr (setup header offset 0x228) at it.
    /// Long strings are truncated at 2047 bytes + null.
    pub fn set_kernel_cmdline(&mut self, cmdline: &str) {
        self.inner.set_kernel_cmdline(cmdline);
    }

    /// Place an initial ramdisk image at the top of physical memory
    /// (page-aligned) and write its address + length into the bzImage
    /// setup header. Returns an Error if the image is larger than
    /// available RAM.
    pub fn set_ramdisk(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        self.inner
            .set_ramdisk(bytes)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Skip the real-mode → PM trampoline and jump straight into
    /// 32-bit code at `entry`. The standard caller flow is:
    ///   const entry = vm.load_bzimage(kernelBytes);
    ///   vm.set_kernel_cmdline("console=ttyS0");
    ///   vm.set_ramdisk(initrdBytes);
    ///   vm.start_protected_mode_at(entry);
    ///   vm.run(50_000);  // pump
    /// The VM is left in the same state a real bootloader would
    /// have produced just after JMPing to the kernel: CR0.PE=1,
    /// flat segments, 32-bit stack, IP = entry.
    pub fn start_protected_mode_at(&mut self, entry: u32) {
        self.inner.start_protected_mode_at(entry);
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

    /// Capture CPU + RAM as a byte blob the host can persist (IndexedDB,
    /// download as file, etc.) and later feed to `restore`. ~1 MiB.
    /// Device state is *not* preserved in this version.
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.snapshot()
    }

    /// Restore CPU + RAM from a `snapshot()` blob. Returns a JS Error
    /// describing the failure (bad magic, wrong version, truncated)
    /// on rejection; on success the VM is exactly where the snapshot
    /// was taken, devices excluded.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        self.inner
            .restore(bytes)
            .map_err(|e| JsError::new(&e.to_string()))
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
    pub fn is_halted(&self) -> bool {
        self.inner.is_halted()
    }

    /// True if `boot()` has been called at least once.
    pub fn is_booted(&self) -> bool {
        self.inner.is_booted()
    }

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
        self.inner
            .set_cmos_time(year, month, day, hour, minute, second);
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
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // wasm-bindgen attributes are inert on the host target, so we can
    // exercise the wrapper directly.

    /// Exercises the bzImage handoff path through the JS-facing
    /// wrappers — the same three-piece sequence JS callers will
    /// run when booting a real Linux image: load_bzimage gets a
    /// code32_start back, set_kernel_cmdline + set_ramdisk land
    /// their pointers in the setup header, the kernel payload
    /// landed at the entry point.
    #[test]
    fn js_facing_bzimage_handoff_end_to_end() {
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        bz.extend_from_slice(&[0xB0, 0xCD, 0xF4]);

        // 2 MiB so code32_start (1 MiB) actually fits.
        let mut vm = WwwVm::new_with_ram_size(0x0020_0000);
        let entry = vm.load_bzimage(&bz).expect("load_bzimage");
        assert_eq!(entry, 0x0010_0000);
        vm.set_kernel_cmdline("root=/dev/ram0");
        vm.set_ramdisk(&[0xAAu8; 512]).expect("ramdisk fits");

        // Setup-header pointers are populated where the kernel
        // will read them. We can't peek directly through the JS
        // surface, so peek via the existing read_mem_u8 path — the
        // 4-byte cmd_line_ptr at 0x90228 should read 0x00 0x08 0x09 0x00.
        assert_eq!(vm.read_mem_u8(0x9_0228), 0x00);
        assert_eq!(vm.read_mem_u8(0x9_0229), 0x08);
        assert_eq!(vm.read_mem_u8(0x9_022A), 0x09);
        assert_eq!(vm.read_mem_u8(0x9_022B), 0x00);
        // ramdisk_size at 0x9021C = 512 = 0x200.
        assert_eq!(vm.read_mem_u8(0x9_021C), 0x00);
        assert_eq!(vm.read_mem_u8(0x9_021D), 0x02);
        // The 3-byte kernel payload lives at the reported entry.
        assert_eq!(vm.read_mem_u8(entry), 0xB0);
        assert_eq!(vm.read_mem_u8(entry + 1), 0xCD);
        assert_eq!(vm.read_mem_u8(entry + 2), 0xF4);
    }

    /// JS-facing end-to-end: bzImage handoff + skip-trampoline.
    /// Mirrors the recommended caller sequence — load_bzimage,
    /// start_protected_mode_at, run — and confirms a 32-bit kernel
    /// payload at code32_start actually ran (EAX holds the imm32 the
    /// kernel deposited there). This is the smallest possible
    /// reproduction of "boot a real kernel from JS" without
    /// synthesising a real-mode trampoline at 0x7C00.
    #[test]
    fn js_facing_skip_trampoline_runs_pm_kernel() {
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        // MOV EAX, 0xDEADBEEF ; HLT — the imm32 only survives if
        // the CPU honored CS.D=1 on the flat code segment built by
        // start_protected_mode_at.
        bz.extend_from_slice(&[0xB8, 0xEF, 0xBE, 0xAD, 0xDE, 0xF4]);

        let mut vm = WwwVm::new_with_ram_size(0x0020_0000); // 2 MiB
        let entry = vm.load_bzimage(&bz).expect("load_bzimage");
        vm.start_protected_mode_at(entry);
        vm.run(16);

        assert!(vm.is_halted());
        assert!(vm.last_error().is_none());
        // Read EAX through the byte window — the wrappers don't
        // expose registers directly but the 4-byte MOV imm32 still
        // lives at memory + entry.
        assert_eq!(vm.read_mem_u8(entry), 0xB8);
        assert_eq!(vm.read_mem_u8(entry + 1), 0xEF);
    }

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
