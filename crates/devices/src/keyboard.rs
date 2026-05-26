//! PS/2 keyboard — minimal 8042-controller surface.
//!
//! What we model:
//!   * Port 0x60 (data) — reads pop the next scan code from the queue;
//!     writes accepted and discarded (no command processing yet).
//!   * Port 0x64 (status/command) — reads return bit 0 (OBF, Output
//!     Buffer Full) reflecting whether the queue has data; writes
//!     accepted and discarded.
//!   * IRQ 1 — level-triggered: asserted whenever the scan-code queue
//!     is non-empty. IoBus polls `irq_pending()` each refresh.
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
        }
    }

    /// Host-side: push a raw scan code byte into the keyboard buffer.
    /// The IRQ line asserts level-high until the byte is drained via
    /// port 0x60.
    pub fn push_scancode(&mut self, code: u8) {
        self.queue.push_back(code);
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
            self.queue.pop_front().unwrap_or(0)
        } else if port == self.status_port {
            // Bit 0 = Output Buffer Full (data available to read).
            // Bit 1 = Input Buffer Full (we're never busy).
            // Higher bits return zero.
            if self.queue.is_empty() {
                0
            } else {
                1
            }
        } else {
            0
        }
    }

    fn write(&mut self, port: u16, _value: u8) {
        // 8042 command-byte processing isn't modeled yet. Writes to
        // 0x60 (host command/data) and 0x64 (controller command) are
        // accepted and dropped — common cases like "disable port" or
        // "self-test" simply don't take effect.
        let _ = port;
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
}
