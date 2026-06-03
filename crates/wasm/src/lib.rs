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
use wwwvm_net::nat::NatStack;
use wwwvm_net::{Allowlist, DnsForwarder, QueueConnector};
use wwwvm_vm::{Stop, Vm};

/// The in-wasm host network stack: the same smoltcp TCP NAT the native build
/// runs, but with the [`QueueConnector`] (no threads/sockets) so each guest
/// flow is tunnelled over a JS WebSocket to `crates/proxy` instead of a real
/// host socket. `Some` once [`WwwVm::net_enable`] is called.
struct NetBridge {
    nat: NatStack,
    conns: QueueConnector,
    /// Monotonic ms clock we feed smoltcp (advanced by the JS pump).
    now_ms: i64,
}

#[wasm_bindgen]
pub struct WwwVm {
    inner: Vm,
    last_error: Option<String>,
    net: Option<NetBridge>,
}

#[wasm_bindgen]
impl WwwVm {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: Vm::new(),
            last_error: None,
            net: None,
        }
    }

    /// Build a VM with a non-default RAM size (default is 1 MiB,
    /// barely enough to load a synthetic boot sector). JS callers
    /// loading a real Linux bzImage need at least ~16 MiB.
    pub fn new_with_ram_size(size: usize) -> Self {
        Self {
            inner: Vm::with_ram_size(size),
            last_error: None,
            net: None,
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

    /// Load the bundled PM-kernel demo: synthetic bzImage with a
    /// 32-bit kernel that prints "Hello from PM!\n" via COM1 and
    /// HLTs. Demonstrates the full bzImage → start_protected_mode_at
    /// → run → read_output chain through the JS surface. RAM is
    /// auto-resized to 2 MiB if currently smaller (code32_start is
    /// at 1 MiB).
    pub fn load_pm_demo(&mut self) -> Result<(), JsError> {
        self.inner
            .load_pm_demo()
            .map_err(|e| JsError::new(&e.to_string()))
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

    /// Advertise a linear framebuffer to the kernel (efifb) so fbcon
    /// renders the console as real RGB pixels — call before
    /// `start_protected_mode_at`. Pair with `console=tty0` on the
    /// cmdline so the VT (and thus fbcon) gets the boot log. The host
    /// reads pixels back with `framebuffer_bytes()` and blits onto a
    /// canvas. 32bpp, 800x600 ≈ 100x37 text cells; pick dims that fit
    /// the VM's RAM near the top (256 MiB is plenty).
    pub fn enable_framebuffer(&mut self, width: u32, height: u32) {
        self.inner
            .enable_linear_framebuffer(width, height, wwwvm_vm::VIDEO_TYPE_EFI);
    }

    /// Whether a framebuffer has been enabled.
    pub fn has_framebuffer(&self) -> bool {
        self.inner.framebuffer_config().is_some()
    }

    /// Framebuffer width in pixels (0 if none enabled).
    pub fn framebuffer_width(&self) -> u32 {
        self.inner.framebuffer_config().map_or(0, |c| c.width)
    }

    /// Framebuffer height in pixels (0 if none enabled).
    pub fn framebuffer_height(&self) -> u32 {
        self.inner.framebuffer_config().map_or(0, |c| c.height)
    }

    /// Bytes per framebuffer scanline (`width * 4`; 0 if none).
    pub fn framebuffer_stride(&self) -> u32 {
        self.inner.framebuffer_config().map_or(0, |c| c.stride)
    }

    /// Snapshot the framebuffer pixels (32bpp, little-endian B,G,R,X;
    /// `stride * height` bytes). Empty when no framebuffer is enabled.
    /// The canvas blitter swaps B,G,R,X → R,G,B,A.
    pub fn framebuffer_bytes(&self) -> Vec<u8> {
        self.inner.framebuffer_bytes().unwrap_or_default()
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

    /// Read a 32-bit little-endian dword from guest RAM. JS hosts
    /// use this to peek at a sentinel a freshly-handed-off kernel
    /// just wrote (e.g. confirming the head_32-shaped demo's
    /// `MOV [0x900], 0xDEADBEEF` step actually ran).
    pub fn read_mem_u32(&self, addr: u32) -> u32 {
        self.inner.read_mem_u32(addr)
    }

    /// Read a 32-bit GPR by its standard x86 encoding index:
    ///   0 EAX, 1 ECX, 2 EDX, 3 EBX, 4 ESP, 5 EBP, 6 ESI, 7 EDI.
    /// Indices outside that range return 0. The kernel just halted?
    /// `vm.read_register_u32(0)` is the natural way for JS to read
    /// the "exit code" the guest stashed in EAX.
    pub fn read_register_u32(&self, idx: u8) -> u32 {
        if idx > 7 {
            return 0;
        }
        self.inner.cpu().read_r32(idx)
    }

    /// Instruction pointer (EIP) after the last step. Useful for
    /// JS debuggers showing "stopped at PC".
    pub fn get_eip(&self) -> u32 {
        self.inner.cpu().ip
    }

    /// EFLAGS bits 0..15. Bit 0 = CF, 6 = ZF, 7 = SF, 9 = IF, 10 = DF,
    /// 11 = OF, etc. JS debuggers display the flag bits next to the
    /// register dump.
    pub fn get_eflags(&self) -> u16 {
        self.inner.cpu().flags
    }

    /// Read a control register by its x86 encoding index:
    ///   0 CR0, 2 CR2, 3 CR3, 4 CR4. Anything else returns 0.
    /// CR1 and CR5..7 are reserved on real silicon and not modelled
    /// here. A JS debugger panel shows CR0.PE/PG, CR2 (last #PF
    /// linear), CR3 (page directory base), CR4.PSE/PGE.
    pub fn read_control_register(&self, idx: u8) -> u32 {
        let cpu = self.inner.cpu();
        match idx {
            0 => cpu.cr0,
            2 => cpu.cr2,
            3 => cpu.cr3,
            4 => cpu.cr4,
            _ => 0,
        }
    }

    /// Read a segment-register selector by standard x86 sreg index:
    ///   0 ES, 1 CS, 2 SS, 3 DS, 4 FS, 5 GS. Anything else returns 0.
    /// A JS debugger panel pairs this with `read_control_register(0)`
    /// to show "PE+CS RPL = current CPL". The selector's hidden
    /// descriptor cache (base / limit / access) isn't surfaced — JS
    /// callers walking the GDT can resolve descriptors themselves
    /// via `read_mem_u32`.
    pub fn read_segment_selector(&self, idx: u8) -> u16 {
        let cpu = self.inner.cpu();
        if idx >= 6 {
            return 0;
        }
        cpu.sregs[idx as usize]
    }

    /// GDTR base — the linear address the kernel's GDT lives at,
    /// loaded via LGDT. JS callers walking descriptors do
    /// `read_mem_u32(gdtr_base + idx * 8)` to fetch each entry.
    pub fn get_gdtr_base(&self) -> u32 {
        self.inner.cpu().gdtr.base
    }

    /// GDTR limit — last valid byte offset into the GDT. A
    /// selector with index << 3 > limit raises #GP(selector).
    pub fn get_gdtr_limit(&self) -> u16 {
        self.inner.cpu().gdtr.limit
    }

    /// IDTR base — the linear address of the IDT. JS debuggers
    /// fetch gate entries via `read_mem_u32(idtr_base + vec * 8)`
    /// (32-bit interrupt/trap gates are 8 bytes).
    pub fn get_idtr_base(&self) -> u32 {
        self.inner.cpu().idtr.base
    }

    /// IDTR limit — last valid byte offset into the IDT. A vector
    /// beyond limit is undefined behavior on real silicon; in
    /// practice Linux sets limit = 0x7FF (256 gates × 8 bytes - 1).
    pub fn get_idtr_limit(&self) -> u16 {
        self.inner.cpu().idtr.limit
    }

    /// Task Register selector. LTR writes this; the cross-ring
    /// dispatch path reads `gdtr.base + (tr & 0xFFF8)` to find
    /// the TSS descriptor and from there the kernel SS0:ESP0.
    /// A debugger UI uses TR to walk to the active TSS.
    pub fn get_tr(&self) -> u16 {
        self.inner.cpu().tr
    }

    /// LAPIC timer Current Count (MMIO 0xFEE0_0390). Counts down
    /// once per CPU step when LVT_TIMER is configured; the kernel
    /// programs Initial Count and watches this register against
    /// TSC for calibration. Returns the live u32 value.
    pub fn get_lapic_current_count(&self) -> u32 {
        self.inner.mem().read_u32(0xFEE0_0390)
    }

    /// LAPIC LVT_TIMER (MMIO 0xFEE0_0320). Vector in bits 7:0,
    /// mask in bit 16, mode in bits 18:17. JS debuggers display
    /// this alongside Current Count so the operator can see what
    /// the kernel programmed.
    pub fn get_lapic_lvt_timer(&self) -> u32 {
        self.inner.mem().read_u32(0xFEE0_0320)
    }

    /// HPET Main Counter low 32 bits (MMIO 0xFED0_00F0). Advances
    /// once per CPU step when General Configuration's ENABLE_CNF
    /// (0xFED0_0010 bit 0) is set. Pair with `get_hpet_counter_high`
    /// for the full 64-bit value.
    pub fn get_hpet_counter_low(&self) -> u32 {
        self.inner.mem().read_u32(0xFED0_00F0)
    }

    /// HPET Main Counter high 32 bits. Usually zero unless the VM
    /// has been running for ~4 billion CPU steps with HPET enabled.
    pub fn get_hpet_counter_high(&self) -> u32 {
        self.inner.mem().read_u32(0xFED0_00F4)
    }

    /// LDT Register selector. LLDT writes this. We don't yet walk
    /// the LDT for descriptor lookups (every test pulls from GDT),
    /// so this is mostly informational — JS debuggers display it
    /// alongside the other selectors so the operator can spot
    /// "kernel did LLDT" events.
    pub fn get_ldtr(&self) -> u16 {
        self.inner.cpu().ldtr
    }

    /// Low 32 bits of the time-stamp counter. JS debuggers show
    /// this in the register panel so users can see the VM ticking.
    /// JS lacks a native u64; the high bits are accessible via
    /// `get_tsc_high()` for callers that want the full 64-bit
    /// value (rare — TSC rolls over 32-bit only after ~4 billion
    /// CPU steps).
    pub fn get_tsc_low(&self) -> u32 {
        self.inner.cpu().tsc as u32
    }

    /// High 32 bits of the TSC. Usually zero in practice.
    pub fn get_tsc_high(&self) -> u32 {
        (self.inner.cpu().tsc >> 32) as u32
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

    /// Step the CPU up to `cycles` times, treating a `STI; HLT` as an idle
    /// wait rather than a terminal halt (the timer keeps ticking and the next
    /// IRQ resumes it). This is the form needed to BOOT a real kernel: Linux
    /// idles on HLT all through boot, so plain `run` would stop immediately.
    /// Returns steps executed; a HLT with interrupts disabled is still
    /// terminal (nothing can wake the CPU).
    pub fn run_idle_aware(&mut self, cycles: u32) -> u32 {
        let (steps, stop) = self.inner.run_steps_idle_aware(cycles);
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

    /// Inject a PS/2 mouse packet (port 0x60 / IRQ 12) so a graphical
    /// guest (Xorg via libinput) sees pointer movement and clicks.
    /// `dx`/`dy` are signed deltas in PS/2 convention (+x right, +y up —
    /// the caller negates the canvas y-delta); `buttons` is a bitmask
    /// (bit0 left, bit1 right, bit2 middle). `i32` params keep JS callers
    /// on plain Numbers (no BigInt marshalling). No-op until the guest
    /// enables mouse reporting.
    pub fn push_mouse_packet(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.inner.push_mouse_packet(
            dx as i16,
            dy as i16,
            buttons & 1 != 0,
            buttons & 2 != 0,
            buttons & 4 != 0,
        );
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

    /// Like `run`, but returns as soon as the guest goes idle (blocked
    /// waiting for an external event such as a NIC frame or a keystroke).
    /// The browser networking loop uses this so it can feed the guest the
    /// next inbound frame promptly instead of letting it spin its whole
    /// budget on the idle HLT.
    pub fn run_until_idle(&mut self, max: u32) -> u32 {
        let (steps, stop) = self.inner.run_steps_until_idle(max);
        if let Stop::CpuError(e) = stop {
            self.last_error = Some(e.to_string());
        }
        steps
    }

    // --- NIC frame bridge (browser networking) ---
    //
    // The guest runs its own TCP/IP over the emulated RTL8139. These expose
    // the L2 frame stream so a browser host can bridge it to the outside
    // world — typically by terminating the guest's TCP and relaying through
    // the WebSocket↔TCP proxy (`crates/proxy`), the same allowlisted path the
    // native build uses with smoltcp. (Native code does the NAT in-process;
    // in the browser it lives in JS/WebAssembly above these calls.)

    /// Take the next Ethernet frame the guest transmitted, or `undefined`
    /// when the TX queue is empty. Call in a loop each tick to drain it.
    pub fn drain_tx_frame(&mut self) -> Option<Vec<u8>> {
        self.inner.drain_tx_frames_one()
    }

    /// Deliver one inbound Ethernet frame (L2, no CRC) to the guest's NIC.
    /// Returns false if RX is disabled or the ring is full (frame dropped) —
    /// the caller should retry the same frame on the next tick.
    pub fn inject_rx_frame(&mut self, frame: &[u8]) -> bool {
        self.inner.inject_rx_frame(frame)
    }

    // --- In-wasm host network stack (smoltcp NAT → WebSocket relay) ---
    //
    // net_enable spins up the SAME smoltcp TCP NAT the native build runs, but
    // with the QueueConnector (no threads/sockets): each guest flow becomes a
    // byte queue JS tunnels over a WebSocket to crates/proxy. net_pump bridges
    // the VM's NIC frames into/out of the NAT (both live in wasm); the JS side
    // only shuttles per-connection payload over WebSockets and resolves names.

    /// Turn on host networking. `allowlist` is the deny-by-default policy
    /// ("host:port", comma-separated; "*" / "host:*" allowed but use specific
    /// hosts in any real deployment — an open relay is dangerous). Gateway is
    /// 10.0.2.2, guest 10.0.2.15 (matching the native console).
    pub fn net_enable(&mut self, allowlist: &str) {
        const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
        const GW_IP: [u8; 4] = [10, 0, 2, 2];
        const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
        let conns = QueueConnector::new();
        let dns = DnsForwarder::new(GW_IP, GW_MAC, Allowlist::parse(allowlist));
        let nat = NatStack::with_connect(GW_IP, GW_MAC, GUEST_IP, dns, conns.connector());
        self.net = Some(NetBridge {
            nat,
            conns,
            now_ms: 0,
        });
    }

    /// Whether host networking has been enabled.
    pub fn net_enabled(&self) -> bool {
        self.net.is_some()
    }

    /// Seed the DNS cache so the guest can resolve `name` → `ips` (packed as
    /// 4-byte IPv4 groups). JS resolves names host-side (proxy / DoH) and
    /// pushes the answers in before the guest queries. Returns how many were
    /// kept (non-routable addresses are dropped). No-op if net isn't enabled.
    pub fn net_cache_dns(&mut self, name: &str, ips: &[u8]) -> usize {
        let Some(net) = self.net.as_mut() else {
            return 0;
        };
        let addrs: Vec<std::net::Ipv4Addr> = ips
            .chunks_exact(4)
            .map(|c| std::net::Ipv4Addr::new(c[0], c[1], c[2], c[3]))
            .collect();
        net.nat.cache_dns(name, &addrs)
    }

    /// Drain the hostnames the guest tried to resolve that aren't cached yet but
    /// are allowlisted (incl. a `*` wildcard). JS resolves these on the fly via
    /// DoH and feeds the answers back through `net_cache_dns`; the guest's
    /// resolver retries and hits the cache. Returns a JSON array of names. This
    /// is what makes an allow-all allowlist work (nothing is pre-resolved).
    pub fn net_take_dns_requests(&mut self) -> String {
        let Some(net) = self.net.as_mut() else {
            return "[]".into();
        };
        let mut s = String::from("[");
        for (i, name) in net.nat.take_dns_requests().into_iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('"');
            s.push_str(&name.replace('\\', "\\\\").replace('"', "\\\""));
            s.push('"');
        }
        s.push(']');
        s
    }

    /// Pump the NAT one tick at monotonic host time `now_ms`: move the guest's
    /// transmitted frames into the stack, advance smoltcp, and inject its
    /// replies into the guest NIC. Call each tick after stepping the CPU.
    pub fn net_pump(&mut self, now_ms: f64) {
        let WwwVm { inner, net, .. } = self;
        let Some(net) = net.as_mut() else {
            return;
        };
        net.now_ms = now_ms as i64;
        while let Some(frame) = inner.drain_tx_frames_one() {
            net.nat.push_guest_frame(frame);
        }
        net.nat.poll(net.now_ms);
        // Stack → guest, bounded; requeue to the front on a full RX ring so
        // ordering is preserved and we retry next tick.
        let mut guard = 1024;
        while guard > 0 {
            let Some(frame) = net.nat.pop_egress() else {
                break;
            };
            if !inner.inject_rx_frame(&frame) {
                net.nat.requeue_egress_front(frame);
                break;
            }
            guard -= 1;
        }
    }

    /// Connections the guest just opened, as JSON:
    /// `[{"id":<n>,"host":"name","ip":"a.b.c.d","port":<n>}, …]` (`"[]"` if
    /// none / disabled). JS opens one WebSocket per id to the proxy and sends
    /// `{host,port}` — `host` is the resolved name (the proxy re-resolves +
    /// allowlists by name); it falls back to the IP if the name is unknown.
    pub fn net_take_new_connections(&self) -> String {
        let Some(net) = self.net.as_ref() else {
            return "[]".into();
        };
        let mut s = String::from("[");
        for (i, (id, ip, port)) in net.conns.take_new().into_iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let host = net.nat.host_for_ip(ip).map(str::to_string);
            let host = host.unwrap_or_else(|| ip.to_string());
            s.push_str(&format!(
                "{{\"id\":{id},\"host\":\"{host}\",\"ip\":\"{ip}\",\"port\":{port}}}"
            ));
        }
        s.push(']');
        s
    }

    /// Guest→host bytes queued for connection `id` (to `ws.send`), or
    /// `undefined` once the NAT has closed the flow — then close the WebSocket
    /// and call `net_conn_closed`. An empty array means "open, nothing yet".
    pub fn net_conn_outbound(&self, id: u32) -> Option<Vec<u8>> {
        let net = self.net.as_ref()?;
        let (bytes, closed) = net.conns.drain_outbound(id);
        if closed && bytes.is_empty() {
            None
        } else {
            Some(bytes)
        }
    }

    /// Connection ids whose guest write side just half-closed (sent FIN), as a
    /// flat list. For each, the embedder should signal the proxy to shut down
    /// the upstream write side (a control frame) WITHOUT closing the WebSocket,
    /// so the host→guest response keeps flowing. Reported once per id.
    pub fn net_take_write_closed(&self) -> Vec<u32> {
        self.net
            .as_ref()
            .map_or_else(Vec::new, |n| n.conns.take_write_closed())
    }

    /// Feed host→guest bytes received on connection `id`'s WebSocket. Returns
    /// false under backpressure (re-queue and retry) or for an unknown id.
    pub fn net_conn_send(&self, id: u32, bytes: &[u8]) -> bool {
        self.net
            .as_ref()
            .is_some_and(|n| n.conns.push_inbound(id, bytes))
    }

    /// Tell the NAT that connection `id`'s WebSocket closed or errored — the
    /// guest gets a FIN and the slot is freed. Idempotent.
    pub fn net_conn_closed(&self, id: u32) {
        if let Some(net) = self.net.as_ref() {
            net.conns.host_closed(id);
        }
    }

    /// Live NATed flow count (diagnostics).
    pub fn net_conn_count(&self) -> u32 {
        self.net.as_ref().map_or(0, |n| n.conns.conn_count() as u32)
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

    /// JS-facing smallest console demo: a PM kernel handed off via
    /// `start_protected_mode_at` writes "Hi!\n" to COM1 (port 0x3F8)
    /// and HLTs. The host drains the UART buffer through
    /// `read_output` and sees the string. This pins the full
    /// chain JS callers actually use to display kernel output:
    /// load_bzimage → start_protected_mode_at → run → read_output.
    #[test]
    fn js_facing_pm_kernel_prints_through_uart() {
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());

        // Kernel: MOV EDX, 0x3F8 (COM1 THR); for each byte of "Hi!\n"
        // MOV AL, byte then OUT DX, AL; HLT. CS.D=1 makes BA take a
        // 32-bit immediate by default.
        //   BA F8 03 00 00   MOV EDX, 0x3F8        (5)
        //   B0 'H' EE        MOV AL,'H'; OUT DX,AL (3)
        //   B0 'i' EE                              (3)
        //   B0 '!' EE                              (3)
        //   B0 0A  EE                              (3)
        //   F4                HLT                  (1)
        let mut payload = Vec::<u8>::new();
        payload.extend_from_slice(&[0xBA, 0xF8, 0x03, 0x00, 0x00]);
        for ch in b"Hi!\n" {
            payload.extend_from_slice(&[0xB0, *ch, 0xEE]);
        }
        payload.push(0xF4);
        bz.extend_from_slice(&payload);

        let mut vm = WwwVm::new_with_ram_size(0x0020_0000);
        let entry = vm.load_bzimage(&bz).expect("load_bzimage");
        vm.start_protected_mode_at(entry);
        vm.run(64);

        assert!(vm.is_halted());
        assert!(vm.last_error().is_none());
        // The string lands in the UART tx buffer; read_output drains it.
        assert_eq!(vm.read_output(), "Hi!\n");
    }

    /// `read_mem_u32` lets JS check a 32-bit sentinel in one call.
    /// A freshly-handed-off kernel that does `MOV [0x900],
    /// 0xCAFEBABE; HLT` should leave the dword readable through
    /// the wrapper — without forcing JS to compose four byte reads.
    #[test]
    fn js_facing_read_mem_u32_reads_kernel_written_sentinel() {
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        // 32-bit kernel payload (CS.D=1 default):
        //   C7 05 00 09 00 00 BE BA FE CA   MOV DWORD [0x900], 0xCAFEBABE
        //   F4                              HLT
        bz.extend_from_slice(&[
            0xC7, 0x05, 0x00, 0x09, 0x00, 0x00, 0xBE, 0xBA, 0xFE, 0xCA, 0xF4,
        ]);

        let mut vm = WwwVm::new_with_ram_size(0x0020_0000);
        let entry = vm.load_bzimage(&bz).expect("load_bzimage");
        vm.start_protected_mode_at(entry);
        vm.run(16);

        assert!(vm.is_halted());
        // Single 32-bit read — would need four read_mem_u8 calls
        // and bit-shifting on the JS side without this wrapper.
        assert_eq!(vm.read_mem_u32(0x900), 0xCAFE_BABE);
    }

    /// The bundled PM-kernel demo: one call sets up the bzImage,
    /// hands off to PM, runs the kernel, and the host drains the
    /// UART tx buffer to read what the kernel printed. This is
    /// the smallest JS-callable shape for "boot a PM kernel and
    /// see its output" — useful for the web demo's PM option.
    #[test]
    fn js_facing_load_pm_demo_prints_hello_from_pm() {
        let mut vm = WwwVm::new();
        vm.load_pm_demo().expect("load_pm_demo");
        vm.run(128);
        assert!(vm.is_halted());
        assert!(vm.last_error().is_none());
        assert_eq!(vm.read_output(), "Hello from PM!\n");
        // The handoff also set up PE in CR0 and ESI = 0x90000.
        assert_eq!(vm.read_control_register(0) & 1, 1);
        assert_eq!(vm.read_register_u32(6), 0x0009_0000);
        // The timer accessors round-trip: the demo kernel doesn't
        // touch LAPIC or HPET, so both stay at construction defaults
        // (zero current count, zero LVT, zero counter).
        assert_eq!(vm.get_lapic_current_count(), 0);
        assert_eq!(vm.get_lapic_lvt_timer(), 0);
        assert_eq!(vm.get_hpet_counter_low(), 0);
        assert_eq!(vm.get_hpet_counter_high(), 0);
    }

    /// Register accessors expose the kernel's post-execution state
    /// directly — no snapshot-blob parsing. JS can call
    /// `read_register_u32(0)` to read EAX as an exit code,
    /// `get_eip()` for the PC, `get_eflags()` for the status word.
    #[test]
    fn js_facing_register_accessors_expose_kernel_state() {
        let mut bz = vec![0u8; 1024];
        bz[0x1F1] = 1;
        bz[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        bz[0x202..0x206].copy_from_slice(b"HdrS");
        bz[0x206..0x208].copy_from_slice(&0x020Au16.to_le_bytes());
        bz[0x214..0x218].copy_from_slice(&0x0010_0000u32.to_le_bytes());
        // 32-bit kernel:
        //   B8 EF BE AD DE   MOV EAX, 0xDEADBEEF
        //   BB 55 AA 55 AA   MOV EBX, 0xAA55AA55
        //   F4               HLT
        bz.extend_from_slice(&[
            0xB8, 0xEF, 0xBE, 0xAD, 0xDE, 0xBB, 0x55, 0xAA, 0x55, 0xAA, 0xF4,
        ]);

        let mut vm = WwwVm::new_with_ram_size(0x0020_0000);
        let entry = vm.load_bzimage(&bz).expect("load");
        vm.start_protected_mode_at(entry);
        vm.run(16);

        assert!(vm.is_halted());
        // GPR-by-index — full 32-bit values.
        assert_eq!(vm.read_register_u32(0), 0xDEAD_BEEF, "EAX (idx 0)");
        assert_eq!(vm.read_register_u32(3), 0xAA55_AA55, "EBX (idx 3)");
        // ESI was set to 0x90000 by start_protected_mode_at.
        assert_eq!(vm.read_register_u32(6), 0x0009_0000, "ESI (idx 6)");
        // EIP points at the byte *after* HLT.
        assert_eq!(vm.get_eip(), entry + 11);
        // IF must be clear (start_protected_mode_at honors §4.1).
        assert_eq!(vm.get_eflags() & 0x0200, 0, "IF clear");
        // Out-of-range index returns 0 (no panic).
        assert_eq!(vm.read_register_u32(42), 0);
        // Control register accessors: CR0 has PE set (start_protected_mode_at
        // flipped it on). CR2/3/4 are zero on this minimal kernel.
        assert_eq!(vm.read_control_register(0) & 1, 1, "CR0.PE");
        assert_eq!(vm.read_control_register(2), 0, "CR2 untouched");
        assert_eq!(vm.read_control_register(3), 0, "CR3 not set by kernel");
        assert_eq!(vm.read_control_register(4), 0, "CR4 not set by kernel");
        // CR1 / CR5+ are reserved/unmodelled — return 0.
        assert_eq!(vm.read_control_register(1), 0);
        assert_eq!(vm.read_control_register(7), 0);
        // Segment selectors. start_protected_mode_at sets CS=0x08
        // (ring-0 code) and DS/ES/FS/GS/SS=0x10 (ring-0 data).
        assert_eq!(vm.read_segment_selector(1), 0x08, "CS");
        assert_eq!(vm.read_segment_selector(0), 0x10, "ES");
        assert_eq!(vm.read_segment_selector(2), 0x10, "SS");
        assert_eq!(vm.read_segment_selector(3), 0x10, "DS");
        assert_eq!(vm.read_segment_selector(4), 0x10, "FS");
        assert_eq!(vm.read_segment_selector(5), 0x10, "GS");
        // Out-of-range index returns 0 (no panic).
        assert_eq!(vm.read_segment_selector(42), 0);
        // GDTR points at the flat-segments GDT start_protected_mode_at
        // builds at 0x500 (null + ring-0 code + ring-0 data → 0x17).
        assert_eq!(vm.get_gdtr_base(), 0x0500);
        assert_eq!(vm.get_gdtr_limit(), 0x0017);
        // IDTR was never loaded — base/limit are at construction defaults.
        assert_eq!(vm.get_idtr_base(), 0);
        assert_eq!(vm.get_idtr_limit(), 0);
        // TSC advanced past zero — at least one step per instruction.
        assert!(vm.get_tsc_low() > 0, "TSC must advance from zero");
        // High half is 0 unless we ran for ~4B cycles.
        assert_eq!(vm.get_tsc_high(), 0);
        // TR / LDTR — never loaded by this minimal kernel, both stay
        // at Cpu::new() defaults.
        assert_eq!(vm.get_tr(), 0);
        assert_eq!(vm.get_ldtr(), 0);
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
