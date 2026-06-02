//! 8042 PS/2 controller — keyboard (port 1) + mouse/AUX (port 2).
//!
//! Enough of the real 8042 protocol for Linux's `i8042` driver to bind,
//! so the built-in `atkbd` (keyboard) and the `psmouse` module attach
//! and the input core hands userspace `/dev/input/event*` (via `evdev`).
//! The earlier stub only answered the controller self-tests and dropped
//! everything else, so `i8042` failed at "Can't read CTR" (it never got
//! a reply to the read-command-byte `0x20`) and no input device bound.
//!
//! Model:
//!   * **Port 0x60 (data)** — reads pop the controller output buffer
//!     (`out`, which carries command/device replies and mouse packets,
//!     each tagged keyboard-vs-aux) and fall back to the scan-code queue
//!     (`queue`) when `out` is empty. Writes are either the data byte of
//!     a pending controller command (write-config / write-to-aux) or, by
//!     default, a keyboard *device* command (reset / identify / set-LEDs…).
//!   * **Port 0x64 (status/command)** — reads return the status byte
//!     (OBF bit0, IBF bit1=0, system-flag bit2, AUX-OBF bit5 = "the byte
//!     waiting at 0x60 is mouse data"). Writes are controller commands
//!     (read/write config byte, enable/disable a port, test, write-to-aux…).
//!
//! IRQ wiring (level-triggered, polled by `IoBus::refresh_irqs`):
//!   * **IRQ 1** asserts while a keyboard byte waits at the head of the
//!     delivery path (a scan code, or a device reply such as the
//!     reset/identify/LED ACKs), the keyboard port is enabled, and the
//!     config byte's keyboard-interrupt bit is set. Device replies DO
//!     raise the line: once `atkbd` attaches, Linux's i8042 is
//!     interrupt-driven and `atkbd`'s connect-time commands wait for the
//!     reply via the interrupt. The config byte's interrupt-enable bit is
//!     the gate that keeps early, poll-only controller replies (self-test,
//!     read-config) from firing a premature IRQ.
//!   * **IRQ 12** asserts while the byte at the head of `out` is mouse
//!     data, the aux port is enabled, and the config byte's aux-interrupt
//!     bit is set. IRQ 12 lives on the slave PIC.
//!
//! Scan-code *content* stays the host's problem: callers push raw Set-1
//! bytes via [`Keyboard::push_scancode`]; translating host key events to
//! scan codes isn't something we can do generically. Mouse motion is
//! injected as ready-made PS/2 packets via [`Keyboard::push_mouse_packet`].

use std::collections::VecDeque;

use crate::IoDevice;

/// What data byte the controller is waiting for after a `0x64` command
/// that takes a parameter on `0x60`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CtrlPending {
    /// Idle — a `0x60` write is a keyboard *device* command.
    None,
    /// `0x60` controller command issued — the next `0x60` write is the
    /// new config byte.
    WriteCcb,
    /// `0xD4` issued — the next `0x60` write is a *mouse* device command.
    WriteAux,
    /// `0xD2` issued — the next `0x60` write is echoed back as keyboard
    /// data (write-to-keyboard-output-buffer diagnostic).
    WriteKbdOut,
    /// `0xD3` issued — the next `0x60` write is echoed back as mouse data
    /// (write-to-aux-output-buffer; this is the AUX_LOOP test i8042 uses
    /// to detect the mouse port).
    WriteAuxOut,
}

pub struct Keyboard {
    data_port: u16,
    status_port: u16,
    /// Raw scan codes (Set-1) pushed by the host. The kernel reads these
    /// via 0x60 (after `out`) and the BIOS INT 0x16 shim drains them via
    /// [`Keyboard::pop_scancode`]; both consume from here. Scan codes
    /// raise IRQ 1.
    queue: VecDeque<u8>,
    /// Controller output buffer: command/device replies and mouse packets,
    /// each tagged `is_aux` so the status byte's AUX-OBF bit and the IRQ
    /// routing know whether the waiting byte is keyboard or mouse data.
    out: VecDeque<(u8, bool)>,
    /// Controller config byte ("command byte" / CCB): bit0 = keyboard
    /// interrupt enable, bit1 = aux interrupt enable, bit2 = system flag,
    /// bit4 = keyboard clock disable, bit5 = aux clock disable, bit6 =
    /// scan-code translation. Default enables the keyboard interrupt and
    /// the system flag so the real-mode/BIOS path and the IRQ test work
    /// before Linux reprograms it.
    ccb: u8,
    /// Keyboard port (port 1) enabled — `0xAE` enables, `0xAD` disables.
    kbd_enabled: bool,
    /// Aux/mouse port (port 2) enabled — `0xA8` enables, `0xA7` disables.
    aux_enabled: bool,
    /// Controller command awaiting its `0x60` data byte.
    pending: CtrlPending,
    /// Keyboard *device* two-byte command awaiting its parameter (the
    /// command code, e.g. `0xED` set-LEDs / `0xF3` typematic / `0xF0`
    /// scan-code-set).
    kbd_arg: Option<u8>,
    /// Mouse *device* two-byte command awaiting its parameter (`0xF3`
    /// sample-rate / `0xE8` resolution).
    mouse_arg: Option<u8>,
    /// Mouse data reporting enabled (`0xF4` enables, `0xF5` disables).
    /// Injected packets are dropped while reporting is off.
    mouse_reporting: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub const DATA_PORT: u16 = 0x60;
    pub const STATUS_PORT: u16 = 0x64;

    /// Keyboard interrupt enable (CCB bit 0).
    const CCB_KBD_INT: u8 = 1 << 0;
    /// Aux interrupt enable (CCB bit 1).
    const CCB_AUX_INT: u8 = 1 << 1;
    /// System flag (CCB bit 2) — mirrored into status bit 2.
    const CCB_SYS_FLAG: u8 = 1 << 2;
    /// Keyboard clock disable (CCB bit 4) — set ⇒ keyboard port off.
    const CCB_KBD_CLK_DIS: u8 = 1 << 4;
    /// Aux clock disable (CCB bit 5) — set ⇒ aux/mouse port off.
    const CCB_AUX_CLK_DIS: u8 = 1 << 5;

    /// Device ACK byte, sent in reply to (almost) every device command.
    const ACK: u8 = 0xFA;

    pub fn new() -> Self {
        Self {
            data_port: Self::DATA_PORT,
            status_port: Self::STATUS_PORT,
            queue: VecDeque::new(),
            out: VecDeque::new(),
            ccb: Self::CCB_KBD_INT | Self::CCB_SYS_FLAG | Self::CCB_AUX_CLK_DIS,
            kbd_enabled: true,
            aux_enabled: false,
            pending: CtrlPending::None,
            kbd_arg: None,
            mouse_arg: None,
            mouse_reporting: false,
        }
    }

    /// Host-side: push a raw scan code byte into the keyboard buffer.
    /// IRQ 1 asserts (level-high) while the queue is non-empty, the
    /// keyboard port is enabled, and the config byte allows it.
    pub fn push_scancode(&mut self, code: u8) {
        self.queue.push_back(code);
    }

    /// Host-side: inject a PS/2 mouse movement/button packet. No-op while
    /// the guest hasn't enabled reporting (`0xF4`) — a real mouse stays
    /// silent until told to stream. Deltas are clamped to the 9-bit
    /// signed range the 3-byte packet can carry.
    pub fn push_mouse_packet(&mut self, dx: i16, dy: i16, left: bool, right: bool, middle: bool) {
        if !self.mouse_reporting {
            return;
        }
        let cx = dx.clamp(-256, 255);
        let cy = dy.clamp(-256, 255);
        let mut b0 = 0x08u8; // bit3 always set on a PS/2 mouse packet
        if left {
            b0 |= 0x01;
        }
        if right {
            b0 |= 0x02;
        }
        if middle {
            b0 |= 0x04;
        }
        if cx < 0 {
            b0 |= 0x10; // X sign
        }
        if cy < 0 {
            b0 |= 0x20; // Y sign
        }
        self.out.push_back((b0, true));
        self.out.push_back(((cx & 0xFF) as u8, true));
        self.out.push_back(((cy & 0xFF) as u8, true));
    }

    /// Drain the next queued scan code. Used by the BIOS INT 0x16 shim.
    pub fn pop_scancode(&mut self) -> Option<u8> {
        self.queue.pop_front()
    }

    /// Peek the next scan code without consuming it (INT 0x16 AH=0x01).
    pub fn peek_scancode(&self) -> Option<u8> {
        self.queue.front().copied()
    }

    pub fn rx_pending(&self) -> usize {
        self.queue.len()
    }

    /// True when the byte waiting at the head of the delivery path is
    /// keyboard data (a scan code), as opposed to a mouse byte. Used to
    /// gate IRQ 1.
    fn head_is_kbd(&self) -> bool {
        match self.out.front() {
            Some(&(_, is_aux)) => !is_aux,
            None => !self.queue.is_empty(),
        }
    }

    /// True when the byte waiting at the head of `out` is mouse data.
    /// Used for the AUX-OBF status bit and to gate IRQ 12.
    fn head_is_aux(&self) -> bool {
        matches!(self.out.front(), Some(&(_, true)))
    }

    /// IRQ 1 (keyboard) — level-asserted while a keyboard byte waits (a
    /// scan code, or a device reply such as the reset/identify ACKs), the
    /// keyboard port is enabled, and the config byte enables the keyboard
    /// interrupt. Device replies DO raise the line: Linux's i8042 is
    /// interrupt-driven once `atkbd` attaches, and `atkbd`'s connect-time
    /// reset/identify wait for the response via the interrupt — not by
    /// polling. The CCB interrupt-enable gate is what keeps early,
    /// poll-only controller replies from firing a premature IRQ (Linux
    /// sets the bit only once it's ready to take interrupts).
    pub fn irq_pending(&self) -> bool {
        self.head_is_kbd() && self.kbd_enabled && (self.ccb & Self::CCB_KBD_INT != 0)
    }

    /// IRQ 12 (mouse/AUX) — level-asserted while mouse data waits at the
    /// head of `out`, the aux port is enabled, and the config byte
    /// enables the aux interrupt.
    pub fn aux_irq_pending(&self) -> bool {
        self.head_is_aux() && self.aux_enabled && (self.ccb & Self::CCB_AUX_INT != 0)
    }

    /// Snapshot: scan-code queue length (u32LE) + bytes. The controller
    /// command/response state is transient (Linux configures it once in
    /// the first millisecond and a later snapshot won't have a command in
    /// flight), so it isn't serialized — `restore` resets it to defaults.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) {
        let len = self.queue.len() as u32;
        out.extend_from_slice(&len.to_le_bytes());
        for b in &self.queue {
            out.push(*b);
        }
    }

    pub fn restore(&mut self, bytes: &[u8]) -> Result<usize, &'static str> {
        if bytes.len() < 4 {
            return Err("kbd: truncated");
        }
        let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if bytes.len() < 4 + n {
            return Err("kbd: truncated queue");
        }
        self.queue = bytes[4..4 + n].iter().copied().collect();
        Ok(4 + n)
    }

    /// Queue a keyboard-sourced reply byte (read back at 0x60, AUX-OBF clear).
    fn reply_kbd(&mut self, b: u8) {
        self.out.push_back((b, false));
    }

    /// Queue a mouse-sourced reply byte (read back at 0x60, AUX-OBF set).
    fn reply_aux(&mut self, b: u8) {
        self.out.push_back((b, true));
    }

    /// Handle a controller command written to port 0x64.
    fn controller_command(&mut self, cmd: u8) {
        match cmd {
            // Read config byte (CCB). Linux's i8042 init does this first;
            // a missing reply is the "Can't read CTR" failure.
            0x20 => {
                let ccb = self.ccb;
                self.reply_kbd(ccb);
            }
            // Write config byte — the next 0x60 write carries it.
            0x60 => self.pending = CtrlPending::WriteCcb,
            // Controller self-test → 0x55 (passed).
            0xAA => self.reply_kbd(0x55),
            // Test keyboard port (port 1) → 0x00 (OK).
            0xAB => self.reply_kbd(0x00),
            // Test aux port (port 2) → 0x00 (OK; we model a mouse).
            0xA9 => self.reply_kbd(0x00),
            // Enable / disable the aux (mouse) port. These also flip the
            // CCB aux-clock-disable bit, because Linux reads the CCB back
            // to confirm the change took — without it i8042 logs "Failed
            // to disable AUX port... Is this a SiS?" and skips the mouse.
            0xA8 => {
                self.aux_enabled = true;
                self.ccb &= !Self::CCB_AUX_CLK_DIS;
            }
            0xA7 => {
                self.aux_enabled = false;
                self.ccb |= Self::CCB_AUX_CLK_DIS;
            }
            // Enable / disable the keyboard port (+ CCB kbd-clock bit).
            0xAE => {
                self.kbd_enabled = true;
                self.ccb &= !Self::CCB_KBD_CLK_DIS;
            }
            0xAD => {
                self.kbd_enabled = false;
                self.ccb |= Self::CCB_KBD_CLK_DIS;
            }
            // Write next 0x60 byte to the aux (mouse) device.
            0xD4 => self.pending = CtrlPending::WriteAux,
            // Write-to-output-buffer diagnostics: the next 0x60 byte is
            // echoed back as keyboard (0xD2) or mouse (0xD3) data. i8042
            // uses the aux variant (AUX_LOOP) to detect the mouse port —
            // without it the port isn't created and psmouse can't bind.
            0xD2 => self.pending = CtrlPending::WriteKbdOut,
            0xD3 => self.pending = CtrlPending::WriteAuxOut,
            // Read input port (0xC0): bit7 set = keyboard not locked (a 0
            // there makes i8042 warn "Keylock active"). Read output port
            // (0xD0): system-reset + A20 lines asserted.
            0xC0 => self.reply_kbd(0x80),
            0xD0 => self.reply_kbd(0x01),
            // Pulse output lines (0xF0..=0xFF) — includes the CPU reset
            // pulse (0xFE). We don't reset from here; accept and drop.
            _ => {}
        }
    }

    /// Handle a keyboard *device* command written to port 0x60.
    fn keyboard_command(&mut self, cmd: u8) {
        // Parameter byte of a pending two-byte command?
        if let Some(prev) = self.kbd_arg.take() {
            self.reply_kbd(Self::ACK);
            // 0xF0 scan-code-set with arg 0 means "report current set".
            if prev == 0xF0 && cmd == 0x00 {
                self.reply_kbd(0x02);
            }
            return;
        }
        match cmd {
            // Reset → ACK then BAT-OK (0xAA).
            0xFF => {
                self.reply_kbd(Self::ACK);
                self.reply_kbd(0xAA);
            }
            // Identify → ACK then MF2 keyboard id (0xAB 0x83).
            0xF2 => {
                self.reply_kbd(Self::ACK);
                self.reply_kbd(0xAB);
                self.reply_kbd(0x83);
            }
            // Set-LEDs / set-typematic / set-scan-code-set: ACK now, the
            // parameter byte follows and is ACKed via `kbd_arg`.
            0xED | 0xF3 | 0xF0 => {
                self.kbd_arg = Some(cmd);
                self.reply_kbd(Self::ACK);
            }
            // Echo → 0xEE (not an ACK).
            0xEE => self.reply_kbd(0xEE),
            // Enable / disable scanning, set defaults, resend, …: plain ACK.
            _ => self.reply_kbd(Self::ACK),
        }
    }

    /// Handle a mouse *device* command written via 0xD4 + 0x60. All
    /// replies are aux-sourced (AUX-OBF set, IRQ 12).
    fn mouse_command(&mut self, cmd: u8) {
        if self.mouse_arg.take().is_some() {
            // Parameter of set-sample-rate / set-resolution → ACK.
            self.reply_aux(Self::ACK);
            return;
        }
        match cmd {
            // Reset → ACK, BAT-OK (0xAA), device id (0x00 = standard mouse).
            0xFF => {
                self.mouse_reporting = false;
                self.reply_aux(Self::ACK);
                self.reply_aux(0xAA);
                self.reply_aux(0x00);
            }
            // Identify → ACK then id 0x00 (plain PS/2 mouse).
            0xF2 => {
                self.reply_aux(Self::ACK);
                self.reply_aux(0x00);
            }
            // Set sample rate / set resolution: ACK, parameter follows.
            0xF3 | 0xE8 => {
                self.mouse_arg = Some(cmd);
                self.reply_aux(Self::ACK);
            }
            // Status request → ACK then 3 status bytes (sane defaults).
            0xE9 => {
                self.reply_aux(Self::ACK);
                self.reply_aux(0x00);
                self.reply_aux(0x02);
                self.reply_aux(0x64);
            }
            // Read data → ACK then a (null) movement packet.
            0xEB => {
                self.reply_aux(Self::ACK);
                self.reply_aux(0x08);
                self.reply_aux(0x00);
                self.reply_aux(0x00);
            }
            // Enable / disable reporting.
            0xF4 => {
                self.mouse_reporting = true;
                self.reply_aux(Self::ACK);
            }
            0xF5 => {
                self.mouse_reporting = false;
                self.reply_aux(Self::ACK);
            }
            // Set scaling / stream / remote mode / set-defaults / resend…: ACK.
            _ => self.reply_aux(Self::ACK),
        }
    }
}

impl IoDevice for Keyboard {
    fn port_range(&self) -> (u16, u16) {
        // We claim 0x60..=0x64 so the bus routes both the data port (0x60)
        // and the status/command port (0x64) here. Ports 0x61–0x63 are
        // PIT/PPI on a real PC; the bus dispatches 0x61 to the PIT before
        // us, and we ignore 0x62/0x63.
        (self.data_port, self.status_port)
    }

    fn read(&mut self, port: u16) -> u8 {
        if port == self.data_port {
            // Controller output buffer first (command/device replies and
            // mouse packets), then queued scan codes.
            if let Some((b, _)) = self.out.pop_front() {
                b
            } else {
                self.queue.pop_front().unwrap_or(0)
            }
        } else if port == self.status_port {
            let mut status = 0u8;
            if !self.out.is_empty() || !self.queue.is_empty() {
                status |= 0x01; // OBF — data available at 0x60
            }
            // bit1 (IBF) — we're never busy.
            if self.ccb & Self::CCB_SYS_FLAG != 0 {
                status |= 0x04; // system flag (POST passed)
            }
            if self.head_is_aux() {
                status |= 0x20; // AUX-OBF — the waiting byte is mouse data
            }
            status
        } else {
            0
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        if port == self.status_port {
            self.controller_command(value);
        } else if port == self.data_port {
            match self.pending {
                CtrlPending::WriteCcb => {
                    self.ccb = value;
                    // Linux enables/disables the ports mainly through the
                    // CCB clock-disable bits, so mirror them into the
                    // port-enabled flags that gate the IRQ lines.
                    self.kbd_enabled = value & Self::CCB_KBD_CLK_DIS == 0;
                    self.aux_enabled = value & Self::CCB_AUX_CLK_DIS == 0;
                    self.pending = CtrlPending::None;
                }
                CtrlPending::WriteAux => {
                    self.pending = CtrlPending::None;
                    self.mouse_command(value);
                }
                CtrlPending::WriteKbdOut => {
                    self.pending = CtrlPending::None;
                    self.reply_kbd(value);
                }
                CtrlPending::WriteAuxOut => {
                    self.pending = CtrlPending::None;
                    self.reply_aux(value);
                }
                CtrlPending::None => self.keyboard_command(value),
            }
        }
        // Writes to 0x62/0x63 are ignored.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_port_drains_in_order() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        kbd.push_scancode(0x9E);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x1E);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x9E);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0); // empty
    }

    #[test]
    fn status_port_reflects_buffer_state() {
        let mut kbd = Keyboard::new();
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 0);
        kbd.push_scancode(0x1C);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 1);
        kbd.read(Keyboard::DATA_PORT);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 0);
    }

    #[test]
    fn irq_pending_tracks_queue() {
        let mut kbd = Keyboard::new();
        assert!(!kbd.irq_pending());
        kbd.push_scancode(1);
        assert!(kbd.irq_pending());
        kbd.read(Keyboard::DATA_PORT);
        assert!(!kbd.irq_pending());
    }

    #[test]
    fn controller_self_test_returns_55() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xAA);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 1);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x55);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 0);
    }

    #[test]
    fn first_port_test_returns_00() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xAB);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x00);
    }

    /// 0xA9 tests the second (aux/mouse) port. We now model a mouse, so
    /// report 0x00 (OK) — telling Linux to proceed with aux init.
    #[test]
    fn second_port_test_returns_00() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xA9);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x00);
    }

    #[test]
    fn command_response_takes_priority_over_scan_codes() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        kbd.write(Keyboard::STATUS_PORT, 0xAA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x55);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x1E);
    }

    /// Read-then-write of the controller config byte (CCB) — the exact
    /// `0x20` / `0x60` round-trip Linux's i8042 init performs. The stub's
    /// failure here was the "Can't read CTR" boot error.
    #[test]
    fn config_byte_read_write_round_trips() {
        let mut kbd = Keyboard::new();
        // Read current CCB (0x20) — must produce a reply byte.
        kbd.write(Keyboard::STATUS_PORT, 0x20);
        assert_eq!(
            kbd.read(Keyboard::STATUS_PORT) & 1,
            1,
            "OBF set for CCB read"
        );
        let ccb = kbd.read(Keyboard::DATA_PORT);
        assert_eq!(ccb & 0x01, 0x01, "default enables the keyboard interrupt");
        // Write a new CCB (0x60 + data) and read it back.
        kbd.write(Keyboard::STATUS_PORT, 0x60);
        kbd.write(Keyboard::DATA_PORT, 0x47);
        kbd.write(Keyboard::STATUS_PORT, 0x20);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x47);
    }

    /// Disabling the keyboard interrupt in the CCB (bit 0) suppresses
    /// IRQ 1 even with a scan code waiting; re-enabling restores it.
    #[test]
    fn config_byte_gates_keyboard_irq() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        assert!(kbd.irq_pending());
        // Clear bit 0.
        kbd.write(Keyboard::STATUS_PORT, 0x60);
        kbd.write(Keyboard::DATA_PORT, 0x00);
        assert!(!kbd.irq_pending(), "kbd int disabled");
        // Set bit 0 again.
        kbd.write(Keyboard::STATUS_PORT, 0x60);
        kbd.write(Keyboard::DATA_PORT, 0x01);
        assert!(kbd.irq_pending(), "kbd int re-enabled");
    }

    /// Disabling the keyboard port (0xAD) suppresses IRQ 1; 0xAE restores it.
    #[test]
    fn port_disable_suppresses_keyboard_irq() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        kbd.write(Keyboard::STATUS_PORT, 0xAD); // disable kbd port
        assert!(!kbd.irq_pending());
        kbd.write(Keyboard::STATUS_PORT, 0xAE); // enable kbd port
        assert!(kbd.irq_pending());
    }

    /// Keyboard reset (0xFF) → ACK (0xFA) then BAT-OK (0xAA), which is the
    /// sequence atkbd polls for when probing the keyboard.
    #[test]
    fn keyboard_reset_acks_then_bat() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::DATA_PORT, 0xFF);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xAA);
    }

    /// Identify (0xF2) → ACK then the MF2 keyboard id 0xAB 0x83.
    #[test]
    fn keyboard_identify_returns_mf2_id() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::DATA_PORT, 0xF2);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xAB);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x83);
    }

    /// Two-byte set-LEDs (0xED + bitmap) ACKs both bytes. The ACKs are
    /// keyboard-sourced and raise IRQ 1 (with the keyboard interrupt
    /// enabled) — `atkbd` waits for them via the interrupt, not by polling.
    #[test]
    fn keyboard_set_leds_acks_both_bytes() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::DATA_PORT, 0xED);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        kbd.write(Keyboard::DATA_PORT, 0x02); // ScrollLock bitmap
        assert!(kbd.irq_pending(), "device ACK raises IRQ 1 for the driver");
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
    }

    /// A full mouse bring-up: enable the aux port, reset, identify, enable
    /// reporting — each device byte is aux-sourced (AUX-OBF set) and the
    /// reset returns ACK / BAT / id.
    #[test]
    fn mouse_reset_identify_enable() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xA8); // enable aux port
                                                // Reset (0xD4 prefixes each aux byte).
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xFF);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 0x20, 0x20, "AUX-OBF set");
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xAA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x00);
        // Identify → ACK + id 0x00.
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xF2);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x00);
        // Enable reporting.
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xF4);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
    }

    /// Set-sample-rate (0xF3 + rate) ACKs both bytes — the IntelliMouse
    /// detection "knock" psmouse sends.
    #[test]
    fn mouse_set_sample_rate_acks_param() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xA8);
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xF3);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xC8); // 200 samples/s
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA);
    }

    /// The AUX_LOOP test (0xD3 + byte) echoes the byte back as mouse data
    /// (AUX-OBF set). i8042 uses this to detect the mouse port; without it
    /// no aux serio port is created and psmouse can't bind. The keyboard
    /// loopback (0xD2) echoes back as keyboard data.
    #[test]
    fn output_buffer_loopback_echoes_with_correct_source() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xD3); // AUX_LOOP
        kbd.write(Keyboard::DATA_PORT, 0x5A);
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 0x20, 0x20, "AUX-OBF set");
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x5A, "byte echoed back");
        kbd.write(Keyboard::STATUS_PORT, 0xD2); // KBD loopback
        kbd.write(Keyboard::DATA_PORT, 0x3C);
        assert_eq!(
            kbd.read(Keyboard::STATUS_PORT) & 0x20,
            0x00,
            "AUX-OBF clear"
        );
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x3C);
    }

    /// An injected packet is dropped until reporting is enabled, then
    /// delivered as 3 aux bytes that assert IRQ 12 (aux port enabled and
    /// the aux interrupt enabled in the CCB).
    #[test]
    fn mouse_packet_gated_on_reporting_and_raises_irq12() {
        let mut kbd = Keyboard::new();
        // Silent before reporting is on.
        kbd.push_mouse_packet(5, -3, true, false, false);
        assert!(kbd.out.is_empty());
        // Enable aux port + aux interrupt + reporting.
        kbd.write(Keyboard::STATUS_PORT, 0xA8);
        kbd.write(Keyboard::STATUS_PORT, 0x60);
        kbd.write(
            Keyboard::DATA_PORT,
            Keyboard::CCB_KBD_INT | Keyboard::CCB_AUX_INT,
        );
        kbd.write(Keyboard::STATUS_PORT, 0xD4);
        kbd.write(Keyboard::DATA_PORT, 0xF4);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFA); // drain the ACK
                                                         // Now a packet flows and raises IRQ 12.
        kbd.push_mouse_packet(5, -3, true, false, false);
        assert!(kbd.aux_irq_pending(), "mouse packet raises IRQ 12");
        let b0 = kbd.read(Keyboard::DATA_PORT);
        assert_eq!(b0 & 0x08, 0x08, "bit3 always set");
        assert_eq!(b0 & 0x01, 0x01, "left button");
        assert_eq!(b0 & 0x20, 0x20, "Y sign (negative dy)");
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 5); // dx
        assert_eq!(kbd.read(Keyboard::DATA_PORT) as i8, -3); // dy
    }

    #[test]
    fn snapshot_round_trip_preserves_scancode_queue_in_order() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        kbd.push_scancode(0x9E);
        kbd.push_scancode(0x1C);

        let mut buf = Vec::new();
        kbd.snapshot_into(&mut buf);
        let mut kbd2 = Keyboard::new();
        let consumed = kbd2.restore(&buf).expect("restore");
        assert_eq!(consumed, 4 + 3, "header + 3 codes");

        assert_eq!(kbd2.read(Keyboard::DATA_PORT), 0x1E);
        assert_eq!(kbd2.read(Keyboard::DATA_PORT), 0x9E);
        assert_eq!(kbd2.read(Keyboard::DATA_PORT), 0x1C);
        assert_eq!(kbd2.read(Keyboard::DATA_PORT), 0);
    }

    #[test]
    fn restore_rejects_truncated_blob() {
        let mut kbd = Keyboard::new();
        assert!(kbd.restore(&[0u8; 3]).is_err(), "truncated header");
        let bad = [10u8, 0, 0, 0, 1, 2];
        assert!(kbd.restore(&bad).is_err(), "truncated queue");
    }
}
