//! PS/2 keyboard — minimal 8042-controller surface.
//!
//! What we model:
//!   * Port 0x60 (data) — reads pop the next byte. Controller command
//!     responses (from a recent 0xAA / 0xAB / 0xA9 on port 0x64) take
//!     priority over scan codes — Linux reads 0x60 immediately after
//!     issuing the command and expects the response there.
//!   * Port 0x64 (status/command) — reads return bit 0 (OBF, Output
//!     Buffer Full) reflecting whether either the response or scan-
//!     code queue has data. Writes select a controller command:
//!     0xAA (self-test → 0x55), 0xAB (port 1 test → 0x00 = OK),
//!     0xA9 (port 2 test → 0xFF = not present). Other commands are
//!     accepted and dropped.
//!   * IRQ 1 — level-triggered: asserted whenever the scan-code queue
//!     is non-empty. IoBus polls `irq_pending()` each refresh. The
//!     command-response path doesn't raise IRQ 1 (Linux polls for it).
//!
//! The scan-code format is opaque to this device: callers push raw
//! bytes via [`Keyboard::push_scancode`] and guests pop them via
//! port-mapped IO. Translating host keystrokes (ASCII, key events,
//! etc.) into Set-1 / Set-2 scan codes is the host's job — it isn't
//! something we can do generically without knowing the layout.

use std::collections::VecDeque;

use crate::IoDevice;

pub struct Keyboard {
    data_port: u16,
    status_port: u16,
    queue: VecDeque<u8>,
    /// Controller command responses, popped from port 0x60 before the
    /// scan-code queue. Transient — not snapshotted, since Linux's
    /// 8042 init issues these commands once in the first millisecond
    /// of boot and any snapshot taken later won't have responses in
    /// flight.
    cmd_response: VecDeque<u8>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub const DATA_PORT: u16 = 0x60;
    pub const STATUS_PORT: u16 = 0x64;

    pub fn new() -> Self {
        Self {
            data_port: Self::DATA_PORT,
            status_port: Self::STATUS_PORT,
            queue: VecDeque::new(),
            cmd_response: VecDeque::new(),
        }
    }

    /// Host-side: push a raw scan code byte into the keyboard buffer.
    /// The IRQ line asserts level-high until the byte is drained via
    /// port 0x60.
    pub fn push_scancode(&mut self, code: u8) {
        self.queue.push_back(code);
    }

    /// Drain the next queued byte. Used by the BIOS INT 0x16 shim;
    /// port 0x60 reads also consume one byte (in the IO dispatch
    /// path). Returns `None` when the queue is empty.
    pub fn pop_scancode(&mut self) -> Option<u8> {
        self.queue.pop_front()
    }

    /// Peek the next byte without consuming it. INT 0x16 AH=0x01
    /// (check keystroke) uses this so the next read still sees the
    /// same byte.
    pub fn peek_scancode(&self) -> Option<u8> {
        self.queue.front().copied()
    }

    pub fn rx_pending(&self) -> usize {
        self.queue.len()
    }

    /// IRQ 1 is level-asserted whenever the buffer has at least one
    /// byte waiting. There's no IER on a PS/2 keyboard at the device
    /// level — the 8042 always interrupts when the buffer fills.
    pub fn irq_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Snapshot: queue length (u32LE) + queue bytes.
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
}

impl IoDevice for Keyboard {
    fn port_range(&self) -> (u16, u16) {
        // The data port (0x60) and status port (0x64) are not
        // contiguous; we claim the full span so the bus dispatch
        // routes both to us and we sort the rest out in read/write.
        // Ports 0x61–0x63 are technically PIT/PPI-related on a real
        // PC; we own them here only because nothing else needs them
        // yet. When the speaker/PPI lands, this range will tighten.
        (self.data_port, self.status_port)
    }

    fn read(&mut self, port: u16) -> u8 {
        if port == self.data_port {
            // Controller responses outrank scan codes: Linux's i8042
            // init reads 0x60 right after writing a command, expecting
            // its specific response there before any keystroke noise.
            if let Some(b) = self.cmd_response.pop_front() {
                b
            } else {
                self.queue.pop_front().unwrap_or(0)
            }
        } else if port == self.status_port {
            // Bit 0 = Output Buffer Full (data available to read in
            // either response or scan-code queue).
            // Bit 1 = Input Buffer Full (we're never busy).
            // Higher bits return zero.
            if self.cmd_response.is_empty() && self.queue.is_empty() {
                0
            } else {
                1
            }
        } else {
            0
        }
    }

    fn write(&mut self, port: u16, value: u8) {
        if port == self.status_port {
            match value {
                // Controller self-test. Linux's i8042_controller_check
                // sends this and times out (1s) if 0x55 doesn't arrive
                // — log message only, init still proceeds.
                0xAA => self.cmd_response.push_back(0x55),
                // First port (keyboard) interface test. 0x00 means OK.
                0xAB => self.cmd_response.push_back(0x00),
                // Second port (aux/mouse) interface test. We don't
                // model an aux port, so report "not present" — Linux
                // skips aux init when the response is non-zero.
                0xA9 => self.cmd_response.push_back(0xFF),
                _ => {
                    // Other commands (port enable/disable, write CCB,
                    // etc.) accepted and dropped — common cases just
                    // don't take effect.
                }
            }
        }
        // Writes to 0x60 (host command/data) are also dropped.
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

    /// Linux's i8042_controller_check sends 0xAA to port 0x64,
    /// waits for OBF, then reads 0x60 expecting 0x55. With no
    /// response the kernel logs a timeout and continues — but it
    /// also disables both 8042 ports as a precaution.
    #[test]
    fn controller_self_test_returns_55() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xAA);
        // Status reports OBF immediately — response is ready.
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 1);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x55);
        // Response consumed — OBF drops.
        assert_eq!(kbd.read(Keyboard::STATUS_PORT) & 1, 0);
    }

    /// 0xAB tests the first (keyboard) port. 0x00 means OK; any
    /// other value tells Linux to skip kbd init.
    #[test]
    fn first_port_test_returns_00() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xAB);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x00);
    }

    /// 0xA9 tests the second (aux/mouse) port. We don't model one,
    /// so report 0xFF (not present) — Linux skips aux init.
    #[test]
    fn second_port_test_returns_ff() {
        let mut kbd = Keyboard::new();
        kbd.write(Keyboard::STATUS_PORT, 0xA9);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0xFF);
    }

    /// Command responses jump ahead of pending scan codes — Linux
    /// reads 0x60 right after the command and expects its response,
    /// not whatever keystroke happened to be in the queue.
    #[test]
    fn command_response_takes_priority_over_scan_codes() {
        let mut kbd = Keyboard::new();
        kbd.push_scancode(0x1E);
        kbd.write(Keyboard::STATUS_PORT, 0xAA);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x55);
        assert_eq!(kbd.read(Keyboard::DATA_PORT), 0x1E);
    }
}
